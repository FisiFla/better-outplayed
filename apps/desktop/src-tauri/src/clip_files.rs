//! Handing a clip's file to the operating system: the clipboard and drag-out.
//!
//! (Reveal-in-folder reuses [`crate::background::reveal_command`].) Everything here acts on a
//! path [`crate::commands::clip_file`] has already vetted: it exists and sits inside the library.
//!
//! The builders are pure so a test pins the exact command for every platform on any host; no
//! test can watch a paste land in Explorer or a drop land in Discord.

use crate::background::FileManager;
use std::path::Path;

/// A process to run, plus the environment it needs.
///
/// The path travels in an environment variable, never spliced into script text: the directory
/// and file name are user-influenced, and PowerShell and AppleScript quoting rules are where
/// injection hides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// The environment variable that carries the path to the clipboard script.
pub const CLIP_PATH_ENV: &str = "REPLAYED_CLIP_PATH";

/// The command that puts `path` on the clipboard as a file (so Explorer, Finder, Discord and
/// mail clients paste the video itself, not its path as text). `None` where this build has no
/// mechanism (generic Linux: clipboards differ per desktop and the target is Windows).
pub fn clipboard_command(path: &Path, manager: FileManager) -> Option<ClipboardCommand> {
    let env = vec![(CLIP_PATH_ENV.to_string(), path.display().to_string())];
    match manager {
        FileManager::Explorer => Some(ClipboardCommand {
            program: "powershell".to_string(),
            args: ["-NoProfile", "-NonInteractive", "-Command"]
                .iter()
                .map(|a| a.to_string())
                .chain([format!("Set-Clipboard -LiteralPath $env:{CLIP_PATH_ENV}")])
                .collect(),
            env,
        }),
        FileManager::MacOs => Some(ClipboardCommand {
            program: "osascript".to_string(),
            args: [
                "-e",
                "set p to system attribute \"REPLAYED_CLIP_PATH\"",
                "-e",
                "set the clipboard to (POSIX file p)",
            ]
            .iter()
            .map(|a| a.to_string())
            .collect(),
            env,
        }),
        FileManager::XdgOpen => None,
    }
}

/// Run [`clipboard_command`] for this host and wait for it; its failure is the error.
pub fn copy_file_to_clipboard(path: &Path) -> Result<(), String> {
    let spec = clipboard_command(path, FileManager::current()).ok_or_else(|| {
        "copying a file to the clipboard is not supported on this platform".to_string()
    })?;
    let output = replayed_media::sidecar_command(&spec.program)
        .args(&spec.args)
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .output()
        .map_err(|err| format!("could not run {}: {err}", spec.program))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("{} failed: {}", spec.program, stderr.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn windows_copies_the_file_through_set_clipboard_without_splicing_the_path() {
        let path = PathBuf::from(r#"C:\Clips\it's a "quote" $(calc).mp4"#);
        let c = clipboard_command(&path, FileManager::Explorer).unwrap();
        assert_eq!(c.program, "powershell");
        assert!(c
            .args
            .last()
            .unwrap()
            .starts_with("Set-Clipboard -LiteralPath $env:"));
        assert!(
            c.args.iter().all(|a| !a.contains("calc")),
            "the path never appears in script text: {:?}",
            c.args
        );
        assert_eq!(
            c.env,
            vec![(CLIP_PATH_ENV.to_string(), path.display().to_string())]
        );
    }

    #[test]
    fn macos_copies_a_posix_file_read_from_the_environment() {
        let c = clipboard_command(Path::new("/Users/a/Clips/x.mp4"), FileManager::MacOs).unwrap();
        assert_eq!(c.program, "osascript");
        assert!(c.args.iter().any(|a| a.contains("POSIX file")));
        assert!(c.args.iter().all(|a| !a.contains("x.mp4")));
        assert_eq!(c.env[0].1, "/Users/a/Clips/x.mp4");
    }

    #[test]
    fn the_script_names_the_variable_the_environment_sets() {
        // The macOS script spells the variable as a literal; keep it in step with the const.
        let c = clipboard_command(Path::new("/x.mp4"), FileManager::MacOs).unwrap();
        assert!(c.args.iter().any(|a| a.contains(CLIP_PATH_ENV)));
    }

    #[test]
    fn other_platforms_are_reported_as_unsupported_not_faked() {
        assert!(clipboard_command(Path::new("/x.mp4"), FileManager::XdgOpen).is_none());
    }
}
