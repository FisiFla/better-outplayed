//! The one-click diagnostics export: what a user pastes into a bug report.
//!
//! # Why this exists
//!
//! The health panel had no buttons at all: a user who wanted to help could only hand-attach a
//! log file by hand, and say nothing about the configuration, the version or the machine. This
//! command assembles all of it, puts it on the clipboard, and hands it back so the panel can
//! show what was copied.
//!
//! # What the report never contains
//!
//! **No credential, ever.** The report carries the configuration and the tail of the log, and
//! both can hold a secret:
//!
//! * the configuration's own credential-ish keys are replaced with `<redacted>`, by key name
//!   (see [`CREDENTIAL_KEYS`]);
//! * the Game State Integration tokens are read from the token files and every occurrence of
//!   them is replaced, wherever they appear — in the configuration, in the log, anywhere;
//! * an `Authorization: Bearer …` header is replaced, because that is how the League client's
//!   own secret travels.
//!
//! The redaction is applied to the text that is **copied**, and to the text that is **returned**:
//! the two are the same string, so what the user pastes and what the panel shows cannot differ.
//!
//! # The clipboard
//!
//! The same safe mechanism the clip copy uses (`clip_files.rs`): the text never appears in
//! script text. It is written to a file, and only the **path** travels in an environment
//! variable, so neither PowerShell's quoting rules nor an apostrophe in the report can turn
//! into an injection. The file is deleted afterwards.

use super::*;
use crate::clip_files::ClipboardCommand;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The environment variable that carries the path of the file holding the text.
///
/// The same rule as the clip copy: the path travels in the environment, never in script text.
pub const DIAGNOSTICS_PATH_ENV: &str = "REPLAYED_DIAGNOSTICS_PATH";

/// How much of the log the report carries: 256 KiB.
///
/// The same bound the health page's reader uses (`health::read_recent_problems`), so the report
/// and the page agree about what "the recent log" is.
const LOG_TAIL_BYTES: u64 = 256 * 1024;

/// The configuration keys whose value is a credential.
///
/// Matched by name, case-insensitively, on the key alone — so `token`, `[events] token` and
/// `some.token` are all caught. The list is deliberately names rather than patterns: a substring
/// rule would catch `tokenizer` or `keyframe`, and a key that merely looks like a secret is
/// better shown than hidden.
pub const CREDENTIAL_KEYS: [&str; 19] = [
    "token",
    "access_token",
    "refresh_token",
    "id_token",
    "gsi_token",
    "secret",
    "secret_key",
    "client_secret",
    "api_key",
    "apikey",
    "api_secret",
    "password",
    "passwd",
    "passphrase",
    "credential",
    "credentials",
    "authorization",
    "private_key",
    "session_key",
];

/// What the report says about the state of the machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DiagnosticsReport {
    /// The application's version.
    pub version: String,
    /// The platform, e.g. `windows 11.0.22631 x86_64`.
    pub platform: String,
    /// When the report was made, in RFC 3339.
    pub generated_at: String,
    /// The effective configuration, **with every credential replaced by `<redacted>`**.
    pub config: String,
    /// The recent problems from the log: the WARN and ERROR lines, oldest first.
    pub problems: Vec<String>,
    /// The tail of the log file, redacted.
    pub log_tail: String,
}

/// Assemble the report, copy it to the clipboard, and hand it back.
///
/// The report is copied and returned together: the panel shows what was copied, so the two can
/// never disagree. A clipboard that refuses is an error, not a silent no-op — the user pressed a
/// button and nothing happened otherwise.
pub fn copy_diagnostics(
    app_data_dir: &Path,
    config_path: &Path,
    log_path: &Path,
) -> Result<DiagnosticsReport, CommandError> {
    let report = build_report(app_data_dir, config_path, log_path)?;
    let text = render(&report);
    copy_text_to_clipboard(&text)?;
    Ok(report)
}

/// The report, without the clipboard.
///
/// Split out so it can be tested without a clipboard: what is in the report is a pure function of
/// the three files.
pub fn build_report(
    app_data_dir: &Path,
    config_path: &Path,
    log_path: &Path,
) -> Result<DiagnosticsReport, CommandError> {
    // The secrets first: the token files' contents are replaced wherever they appear, so a token
    // that was pasted into the configuration is caught as well.
    let secrets = read_secrets(app_data_dir);
    let config_text = std::fs::read_to_string(config_path).unwrap_or_else(|_| {
        format!(
            "(no config file at {}; the values in config.example.toml are in force)",
            config_path.display()
        )
    });
    let config = redact(&redact_keys(&config_text), &secrets);
    let log_tail = redact(&read_log_tail(log_path), &secrets);
    // The problems come from the health page's own reader, so the report and the panel agree.
    let problems = crate::health::read_recent_problems(log_path, 50)
        .into_iter()
        .map(|line| redact(&line, &secrets))
        .collect();
    Ok(DiagnosticsReport {
        version: env!("CARGO_PKG_VERSION").to_string(),
        platform: platform(),
        generated_at: now_rfc3339(),
        config,
        problems,
        log_tail,
    })
}

/// The report as the text the user pastes.
fn render(report: &DiagnosticsReport) -> String {
    let mut text = String::new();
    text.push_str(&format!("replayed {} diagnostics\n", report.version));
    text.push_str(&format!("platform:  {}\n", report.platform));
    text.push_str(&format!("generated: {}\n", report.generated_at));
    text.push_str("\n--- recent problems ---\n");
    if report.problems.is_empty() {
        text.push_str("(none in the log's tail)\n");
    } else {
        for problem in &report.problems {
            text.push_str(problem);
            text.push('\n');
        }
    }
    text.push_str("\n--- configuration (credentials redacted) ---\n");
    text.push_str(&report.config);
    text.push_str("\n--- log tail ---\n");
    text.push_str(&report.log_tail);
    text
}

/// The token files' contents: the secrets that must never appear in the report.
///
/// Read from the application data directory, where the listener keeps them
/// (`gsi-token-cs2.txt` and `gsi-token-dota2.txt`). A file that is not there is not an error:
/// the listener generates one when it starts.
fn read_secrets(app_data_dir: &Path) -> Vec<String> {
    ["gsi-token-cs2.txt", "gsi-token-dota2.txt", "gsi-token.txt"]
        .into_iter()
        .filter_map(|name| std::fs::read_to_string(app_data_dir.join(name)).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .collect()
}

/// The tail of the log file, at most [`LOG_TAIL_BYTES`].
///
/// The health page reads the same tail for its problems; the report needs the tail itself, so
/// the read is here rather than in `health.rs` (which is the page's, and answers a different
/// question).
fn read_log_tail(log_path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(log_path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file
        .seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL_BYTES)))
        .is_err()
    {
        return String::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The configuration's credential keys, replaced.
///
/// A line whose key is one of [`CREDENTIAL_KEYS`] keeps its key and loses its value. The text is
/// handled as text rather than as TOML on purpose: a configuration that does not parse is
/// exactly the one a user needs to send, and a parser that refuses it would leave the report
/// without the very file that is broken.
fn redact_keys(text: &str) -> String {
    text.lines()
        .map(|line| {
            let Some((key, _)) = line.split_once('=') else {
                return line.to_string();
            };
            let name = key.trim().trim_matches('"');
            if CREDENTIAL_KEYS
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(name))
            {
                let indent = &line[..line.len() - line.trim_start().len()];
                format!("{indent}{} = \"<redacted>\"", key.trim())
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every occurrence of a secret, replaced.
///
/// The token is a fixed string of hex, so a plain substring replacement is exact: it cannot
/// match part of a longer word by accident in a way that would matter, and it does match the
/// token wherever it appears — in a value, in a comment, in a log line.
fn redact(text: &str, secrets: &[String]) -> String {
    let mut redacted = text.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            redacted = redacted.replace(secret.as_str(), "<redacted>");
        }
    }
    // The League client's own secret travels in an `Authorization: Bearer …` header.
    redact_bearer(&redacted)
}

/// A bearer token, replaced by whatever follows the scheme.
fn redact_bearer(text: &str) -> String {
    const MARKER: &str = "Bearer ";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(MARKER) {
        out.push_str(&rest[..at]);
        out.push_str(MARKER);
        let after = &rest[at + MARKER.len()..];
        let end = after
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .unwrap_or(after.len());
        out.push_str("<redacted>");
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------------------
// The clipboard
// ---------------------------------------------------------------------------------------

/// The command that puts a file's contents on the clipboard.
///
/// The path travels in an environment variable and the script reads the file: the text itself
/// never appears in script text, so nothing in it can be read as script.
#[cfg(windows)]
fn clipboard_command(path: &Path) -> Option<ClipboardCommand> {
    Some(ClipboardCommand {
        program: "powershell".to_string(),
        args: ["-NoProfile", "-NonInteractive", "-Command"]
            .iter()
            .map(|a| a.to_string())
            .chain([format!(
                "Set-Clipboard -Value (Get-Content -Raw -LiteralPath $env:{DIAGNOSTICS_PATH_ENV})"
            )])
            .collect(),
        env: vec![(DIAGNOSTICS_PATH_ENV.to_string(), path.display().to_string())],
    })
}

/// The command that puts a file's contents on the clipboard.
#[cfg(target_os = "macos")]
fn clipboard_command(path: &Path) -> Option<ClipboardCommand> {
    Some(ClipboardCommand {
        program: "osascript".to_string(),
        args: [
            "-e",
            "set p to system attribute \"REPLAYED_DIAGNOSTICS_PATH\"",
            "-e",
            "set the clipboard to (read (POSIX file p))",
        ]
        .iter()
        .map(|a| a.to_string())
        .collect(),
        env: vec![(DIAGNOSTICS_PATH_ENV.to_string(), path.display().to_string())],
    })
}

/// The command that puts a file's contents on the clipboard.
///
/// Nowhere: the clipboards differ per desktop and there is no one command to run.
#[cfg(not(any(windows, target_os = "macos")))]
fn clipboard_command(_path: &Path) -> Option<ClipboardCommand> {
    None
}

/// Put `text` on the system clipboard.
///
/// The text goes into a file first and the file is read by the clipboard command, so a report
/// full of quotes, newlines and apostrophes cannot become script. The file is removed
/// afterwards, whatever the outcome.
pub fn copy_text_to_clipboard(text: &str) -> Result<(), CommandError> {
    let path = write_temp_file(text)?;
    let outcome = copy_file_contents_to_clipboard(&path);
    let _ = std::fs::remove_file(&path);
    outcome
}

/// The clipboard copy, with the file already written.
fn copy_file_contents_to_clipboard(path: &Path) -> Result<(), CommandError> {
    let Some(spec) = clipboard_command(path) else {
        return Err(CommandError::new(
            ErrorCode::Io,
            "copying to the clipboard is not supported on this platform".to_string(),
        ));
    };
    let output = replayed_media::sidecar_command(&spec.program)
        .args(&spec.args)
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .output()
        .map_err(|err| {
            CommandError::new(
                ErrorCode::Io,
                format!(
                    "could not run {} to reach the clipboard: {err}",
                    spec.program
                ),
            )
        })?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(CommandError::new(
            ErrorCode::Io,
            format!(
                "{} could not reach the clipboard: {}",
                spec.program,
                stderr.trim()
            ),
        ))
    }
}

/// The file the text is written to, in the system's temporary directory.
///
/// A name that carries the process id, so two copies at once cannot share it.
fn write_temp_file(text: &str) -> Result<PathBuf, CommandError> {
    let mut path = std::env::temp_dir();
    path.push(format!("replayed-diagnostics-{}.txt", std::process::id()));
    std::fs::write(&path, text).map_err(|err| {
        CommandError::new(
            ErrorCode::Io,
            format!(
                "could not write the diagnostics file {}: {err}",
                path.display()
            ),
        )
    })?;
    Ok(path)
}

// ---------------------------------------------------------------------------------------
// The platform and the clock
// ---------------------------------------------------------------------------------------

/// The platform the application is running on.
///
/// `windows 11.0.22631 x86_64` on Windows: the version is the kernel's own, read through
/// `RtlGetVersion`, which is the only call that does not lie about it (`GetVersionEx` reports
/// 6.2 to an application with no manifest). Windows 11 is the version 10.0 with a build number
/// of 22000 or more, which is how it is told apart.
pub fn platform() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    #[cfg(windows)]
    {
        format!("windows {} {arch}", windows_version())
    }
    #[cfg(not(windows))]
    {
        let _ = os;
        format!("{os} {arch}")
    }
}

/// The Windows version, as `major.minor.build`.
#[cfg(windows)]
fn windows_version() -> String {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    // SAFETY: `RtlGetVersion` fills the struct this function owns, and the struct is zeroed
    // first so every field is defined before it is read.
    let mut info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
    // SAFETY: as above.
    let status = unsafe { RtlGetVersion(&mut info) };
    // `NTSTATUS` is a newtype over the status code, and zero is success.
    if status.0 != 0 {
        return "unknown".to_string();
    }
    let major = info.dwMajorVersion;
    let minor = info.dwMinorVersion;
    let build = info.dwBuildNumber;
    // Windows 11 is 10.0 with a build of 22000 or more; the marketing version is what a user
    // will recognise.
    let major = if major == 10 && build >= 22000 {
        11
    } else {
        major
    };
    format!("{major}.{minor}.{build}")
}

/// The current time, in RFC 3339 (`2026-10-20T09:41:07Z`).
///
/// Computed here rather than through a date library: the shell has no use for one, and a
/// timestamp is arithmetic.
pub fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let (year, month, day, hour, minute, second) = civil_from_unix(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// The civil date and time of a Unix timestamp, in UTC.
///
/// Howard Hinnant's `civil_from_days`, the standard conversion: days since the epoch, shifted
/// to an era that starts on 1 March 2000 so the leap day is the last day of the year.
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (
        year,
        m,
        d,
        (rem / 3_600) as u32,
        ((rem % 3_600) / 60) as u32,
        (rem % 60) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token, not a credential: the digits of pi in hex, the same in every test. `gitleaks:allow`
    /// because a hex string assigned to a name containing "token" reads as a leak.
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"; // gitleaks:allow

    /// A report of a machine with a config file, a log file and a token file.
    fn machine() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            "[encode]\nvendor = \"auto\"\nbitrate_kbps = 20000\n\n[events]\ngsi_port = 45671\n",
        )
        .unwrap();
        let log = dir.path().join("replayed.log");
        std::fs::write(
            &log,
            "2026-10-20T09:00:00Z  INFO a: started\n\
             2026-10-20T09:00:01Z  WARN a: the encoder was slow\n\
             2026-10-20T09:00:02Z ERROR a: the capture failed\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("gsi-token-cs2.txt"), format!("{TOKEN}\n")).unwrap();
        let app_data = dir.path().to_path_buf();
        (dir, config, log, app_data)
    }

    /// The report carries the version, the platform, the configuration and the log's problems.
    #[test]
    fn the_report_carries_the_state_of_the_machine() {
        let (_dir, config, log, app_data) = machine();
        let report = build_report(&app_data, &config, &log).unwrap();

        assert_eq!(report.version, env!("CARGO_PKG_VERSION"));
        assert!(report.platform.contains(std::env::consts::ARCH));
        assert!(
            report.generated_at.ends_with('Z') && report.generated_at.contains('T'),
            "an RFC 3339 timestamp: {}",
            report.generated_at
        );
        assert!(
            report.config.contains("vendor = \"auto\""),
            "{}",
            report.config
        );
        assert_eq!(report.problems.len(), 2, "the WARN and the ERROR line");
        assert!(report.log_tail.contains("the capture failed"));
    }

    /// The token never appears in the report, wherever it was.
    ///
    /// The token is written into the configuration here, which is the way it would leak: a user
    /// pastes it into a file and the file goes into a bug report.
    #[test]
    fn the_gsi_token_never_appears_in_the_report() {
        let (dir, config, log, app_data) = machine();
        let text = std::fs::read_to_string(&config).unwrap();
        std::fs::write(&config, format!("{text}\ngsi_token = \"{TOKEN}\"\n")).unwrap();

        let report = build_report(&app_data, &config, &log).unwrap();

        assert!(
            !report.config.contains(TOKEN),
            "the token is not in the configuration: {}",
            report.config
        );
        assert!(
            !report.log_tail.contains(TOKEN),
            "nor in the log: {}",
            report.log_tail
        );
        assert!(
            report.config.contains("<redacted>"),
            "and it is replaced, not merely absent: {}",
            report.config
        );
        let _ = dir;
    }

    /// A credential-ish key is replaced by its name, wherever it is in the file.
    #[test]
    fn a_credential_key_is_replaced_wherever_it_is() {
        let text = "token = \"abc\"\n[events]\napi_key = \"def\"\npassword = \"ghi\"\n\
                    vendor = \"auto\"\nsecret_key = \"jkl\"\n";
        let redacted = redact_keys(text);
        assert!(!redacted.contains("abc"));
        assert!(!redacted.contains("def"));
        assert!(!redacted.contains("ghi"));
        assert!(!redacted.contains("jkl"), "{redacted}");
        // A key that is not a credential survives.
        assert!(redacted.contains("vendor = \"auto\""), "{redacted}");
        // And the key itself is still there, so the reader can see what was redacted.
        assert!(redacted.contains("token = \"<redacted>\""), "{redacted}");
    }

    /// A bearer token is replaced: that is how the League client's own secret travels.
    #[test]
    fn a_bearer_token_is_replaced() {
        let text = "Authorization: Bearer abcdef123456\nand Bearer xyz again";
        let redacted = redact_bearer(text);
        assert!(!redacted.contains("abcdef123456"), "{redacted}");
        assert!(!redacted.contains("xyz"), "{redacted}");
        assert!(redacted.contains("<redacted>"), "{redacted}");
    }

    /// The clipboard command carries the path in the environment and never the text.
    ///
    /// The whole point of the file-and-environment dance: a report full of quotes and
    /// apostrophes must not be able to become script.
    #[test]
    fn the_clipboard_command_carries_the_path_and_never_the_text() {
        let path = Path::new("/tmp/replayed-diagnostics.txt");
        let Some(spec) = clipboard_command(path) else {
            // No clipboard on this platform: the report is still assembled.
            return;
        };
        assert!(
            spec.args
                .iter()
                .all(|a| !a.contains("replayed-diagnostics")),
            "the path is never in the script text: {:?}",
            spec.args
        );
        assert!(
            spec.args.iter().any(|a| a.contains(DIAGNOSTICS_PATH_ENV)),
            "the script reads the environment: {:?}",
            spec.args
        );
        assert_eq!(
            spec.env,
            vec![(DIAGNOSTICS_PATH_ENV.to_string(), path.display().to_string())],
            "and the environment carries the path: {:?}",
            spec.env
        );
        assert!(!spec.program.is_empty());
    }

    /// The timestamp is a real date, not an offset from the epoch.
    #[test]
    fn the_timestamp_is_a_civil_date() {
        // 2026-10-20T09:41:07Z, as a Unix timestamp.
        assert_eq!(civil_from_unix(1_792_489_267), (2026, 10, 20, 9, 41, 7));
        // The epoch itself.
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // A leap day.
        assert_eq!(civil_from_unix(1_709_164_800), (2024, 2, 29, 0, 0, 0));
    }

    /// A machine with no config file and no log still produces a report.
    #[test]
    fn a_machine_with_no_files_still_produces_a_report() {
        let dir = tempfile::tempdir().unwrap();
        let report = build_report(
            dir.path(),
            &dir.path().join("config.toml"),
            &dir.path().join("replayed.log"),
        )
        .unwrap();
        assert!(
            report.config.contains("no config file"),
            "{}",
            report.config
        );
        assert!(report.log_tail.is_empty());
        assert!(report.problems.is_empty());
    }
}
