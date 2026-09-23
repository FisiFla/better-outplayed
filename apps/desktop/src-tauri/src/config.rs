//! Where the desktop shell reads its storage settings from, and where it keeps its files.
//!
//! # Why this is not `localplay_cli::config::Config`
//!
//! The CLI owns the full configuration model (spec §10) and this module deliberately does
//! not duplicate it. Two reasons, both load-bearing:
//!
//! 1. The desktop shell needs exactly one section, `[storage]`. It captures nothing, so
//!    `[buffer]`, `[encode]`, `[audio]`, `[hotkeys]` and `[events]` are none of its
//!    business — and re-validating them here would let a capture-only mistake (say
//!    `vendor = "software"`) refuse to open the review UI, which has no encoder in it.
//! 2. Depending on `localplay-cli` to borrow its `Config` would pull the whole capture
//!    stack — `wgc`, `wasapi`, the ffmpeg encoder child — into the GUI binary for the sake
//!    of three fields.
//!
//! What must NOT drift is the *location* the two binaries agree on, which is why
//! [`app_data_dir`] repeats the CLI's rule verbatim instead of inventing one: the CLI
//! writes `localplay.db` in that directory and the shell reads the same file. A test below
//! pins the parse of the repository's real `config.example.toml` against that section, so
//! a change to the example cannot silently diverge from what this module deserialises.
//!
//! A full adapter that reads the whole file through the CLI's own type would be the better
//! end state; it is a bigger change than this pass should make to a crate that already
//! works, and it is named here rather than done.

use crate::commands::{CommandError, ErrorCode};
use localplay_recorder::config::{BufferSection, EncodeSection, StorageSection};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The repository's example config, compiled in as the fallback for a fresh install.
///
/// The path is relative to *this file*: `src/config.rs` → `src-tauri/src/` → `src-tauri/` →
/// `desktop/` → `apps/` → repo root.
pub const EXAMPLE_CONFIG: &str = include_str!("../../../../config.example.toml");

/// The `[storage]` section of the config file (spec §10).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StorageConfig {
    /// Empty means "the `clips` directory under the application data directory".
    pub clips_dir: String,
    /// Total bytes the clips directory may occupy, favourites included (spec §8.1).
    pub max_total_bytes: u64,
    /// A non-favourited clip older than this many days is deleted (spec §8.1).
    pub max_age_days: u64,
}

impl StorageConfig {
    /// The settings from the repository's example config — the fallback when no
    /// `config.toml` exists yet, so a first run behaves exactly as the example documents.
    pub fn example() -> Result<Self, CommandError> {
        Self::from_toml(EXAMPLE_CONFIG)
    }

    pub fn from_toml(text: &str) -> Result<Self, CommandError> {
        // Only `[storage]` is required: an otherwise-capture-shaped file is still valid
        // here, and a file whose `[storage]` is missing or mis-typed is not.
        #[derive(Deserialize)]
        struct File {
            storage: StorageConfig,
        }
        toml::from_str::<File>(text)
            .map(|f| f.storage)
            .map_err(|err| {
                CommandError::new(
                    ErrorCode::InvalidInput,
                    format!("the config file's [storage] section could not be read: {err}"),
                )
            })
    }

    /// Read the config at `path`, or fall back to the example when it does not exist.
    ///
    /// A file that exists but is malformed is an error rather than a silent fallback: a
    /// user who edited their cap and typo'd it must not be shown an app quietly behaving
    /// as if they had not. This matches the CLI, which also refuses to start on a bad file.
    pub fn load(path: &Path) -> Result<Self, CommandError> {
        if !path.is_file() {
            tracing::info!(
                "no config at {}; using the values from config.example.toml",
                path.display()
            );
            return Self::example();
        }
        let text = std::fs::read_to_string(path).map_err(|err| {
            CommandError::new(
                ErrorCode::Io,
                format!("could not read the config file {}: {err}", path.display()),
            )
        })?;
        Self::from_toml(&text)
    }
}

/// The `[buffer]`, `[encode]` and `[storage]` sections — everything the recorder needs to
/// start.
///
/// Why this is separate from [`StorageConfig`]: the review pane reads `[storage]` when the
/// window opens, while a *recording* needs `[buffer]` and `[encode]` as well, and the two
/// are deliberately parsed at different moments. The shell must open even when the capture
/// sections are missing or wrong (a file carrying only `[storage]`, a typo'd
/// `vendor = "software"`) — that is the point of a review pane that works without a
/// recorder — so this is parsed when *recording starts*, and a failure there fails that
/// command rather than the window.
///
/// The section types are the recorder's own (`localplay_recorder::config`), which the CLI
/// deserialises too, so one `config.toml` means one thing to both front-ends.
#[derive(Debug, Clone, Deserialize)]
pub struct RecordingConfig {
    pub buffer: BufferSection,
    pub encode: EncodeSection,
    pub storage: StorageSection,
}

impl RecordingConfig {
    /// The capture sections of the repository's example config — the fallback for a fresh
    /// install, so a first recording behaves exactly as the example documents.
    pub fn example() -> Result<Self, CommandError> {
        Self::from_toml(EXAMPLE_CONFIG)
    }

    pub fn from_toml(text: &str) -> Result<Self, CommandError> {
        toml::from_str::<Self>(text).map_err(|err| {
            CommandError::new(
                ErrorCode::InvalidInput,
                format!(
                    "the config file's [buffer], [encode] and [storage] sections could not \
                     be read, so a recording cannot be started: {err}"
                ),
            )
        })
    }

    /// Read the recording settings from `path`, or fall back to the example when the file
    /// does not exist.
    ///
    /// A file that exists but is malformed is an error rather than a silent fallback: a
    /// user who edited `encode.fps` and typo'd it must not be recorded at a rate they did
    /// not ask for.
    pub fn load(path: &Path) -> Result<Self, CommandError> {
        if !path.is_file() {
            tracing::info!(
                "no config at {}; recording with the values from config.example.toml",
                path.display()
            );
            return Self::example();
        }
        let text = std::fs::read_to_string(path).map_err(|err| {
            CommandError::new(
                ErrorCode::Io,
                format!("could not read the config file {}: {err}", path.display()),
            )
        })?;
        Self::from_toml(&text)
    }
}

/// The `[hotkeys]` and `[app]` sections — what the shell's *background* half needs.
///
/// The third reader of the same file (see the module docs for why there is more than one):
/// `[storage]` opens the review window, `[buffer]`/`[encode]`/`[storage]` start a recording,
/// and these two install the global clip hotkey and, optionally, the start-with-system
/// entry. The values are *read once at startup* and are not re-read when a window opens: a
/// hotkey is registered once and owned until the process exits, which is exactly why the
/// tray offers "open config file" and the panel shows the path it came from.
///
/// Both sections are optional **here**, deliberately: a `config.toml` a user wrote for the
/// CLI carries them, but a hand-trimmed file that does not is still a working window with
/// the documented default chord rather than a shell that refuses to open. The CLI's own
/// `Config` requires both; this is the front-end that can default them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundConfig {
    /// `[hotkeys] clip`, verbatim. Parsed by `localplay_events::hotkey::Hotkey` — the same
    /// parser the CLI uses — so one file means one chord to both front-ends.
    pub clip_hotkey: String,
    /// `[app] start_with_system`: whether the shell should own a start-with-Windows entry.
    pub start_with_system: bool,
}

/// The chord used when `config.toml` has no `[hotkeys]` section.
///
/// A copy of `config.example.toml`'s value on purpose — the example is what the CLI falls
/// back to and what a fresh install is documented to do — and pinned to it by a test below,
/// because two spellings of "the default" that disagree is exactly the kind of drift a
/// default exists to prevent. (It cannot be read from the example at *this* point without a
/// parse that would need the same default.)
pub const DEFAULT_CLIP_HOTKEY: &str = "Ctrl+F8";

impl BackgroundConfig {
    /// The settings from the repository's example config — the fallback when no
    /// `config.toml` exists yet.
    pub fn example() -> Result<Self, CommandError> {
        Self::from_toml(EXAMPLE_CONFIG)
    }

    pub fn from_toml(text: &str) -> Result<Self, CommandError> {
        #[derive(Deserialize)]
        struct File {
            hotkeys: Option<HotkeySection>,
            #[serde(default)]
            app: Option<AppSection>,
        }
        #[derive(Deserialize)]
        struct HotkeySection {
            clip: String,
        }
        #[derive(Deserialize)]
        struct AppSection {
            #[serde(default)]
            start_with_system: bool,
        }

        let file: File = toml::from_str(text).map_err(|err| {
            CommandError::new(
                ErrorCode::InvalidInput,
                format!(
                    "the config file's [hotkeys] section could not be read, so the clip \
                     hotkey cannot be installed: {err}"
                ),
            )
        })?;
        Ok(Self {
            clip_hotkey: file.hotkeys.map(|h| h.clip).unwrap_or_else(|| {
                tracing::info!(
                    "config.toml has no [hotkeys] section; using the documented default \
                     \"{DEFAULT_CLIP_HOTKEY}\""
                );
                DEFAULT_CLIP_HOTKEY.to_string()
            }),
            start_with_system: file.app.map(|a| a.start_with_system).unwrap_or(false),
        })
    }

    /// Read the background settings from `path`, or fall back to the example when the file
    /// does not exist. A file that exists but is malformed is an error, as everywhere else
    /// in this module: a typo'd hotkey must be reported, not silently replaced.
    pub fn load(path: &Path) -> Result<Self, CommandError> {
        if !path.is_file() {
            tracing::info!(
                "no config at {}; the clip hotkey and [app] settings come from \
                 config.example.toml",
                path.display()
            );
            return Self::example();
        }
        let text = std::fs::read_to_string(path).map_err(|err| {
            CommandError::new(
                ErrorCode::Io,
                format!("could not read the config file {}: {err}", path.display()),
            )
        })?;
        Self::from_toml(&text)
    }
}

/// The application data directory, by the same rule the CLI uses.
///
/// Duplicated on purpose (see the module docs): the two binaries must agree on this path
/// or the shell would show an empty library while the recorder writes clips somewhere
/// else. Windows uses `%LOCALAPPDATA%`; the macOS development host
/// `~/Library/Application Support` via `XDG_DATA_HOME` if that is set, and the current
/// directory as the last resort, which is what the CLI does.
pub fn app_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("localplay")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_storage_section_of_the_example_config() {
        // The example config is the fallback for a fresh install, so this is the exact
        // behaviour a first run gets. It also pins this module to the repository's real
        // file: if `[storage]` is renamed or re-typed there, this fails here.
        let cfg = StorageConfig::example().expect("config.example.toml must stay readable");
        assert_eq!(cfg.clips_dir, "", "the example leaves the clips directory to the app dir");
        assert_eq!(cfg.max_total_bytes, 53_687_091_200, "50 GiB, as spec §10 documents");
        assert_eq!(cfg.max_age_days, 7);
    }

    #[test]
    fn accepts_a_file_that_only_carries_the_storage_section() {
        // The other sections are the recorder's business; their absence must not stop the
        // review UI from opening.
        let cfg = StorageConfig::from_toml(
            r#"
            [storage]
            clips_dir = "/tmp/clips"
            max_total_bytes = 1024
            max_age_days = 2
            "#,
        )
        .unwrap();
        assert_eq!(cfg.clips_dir, "/tmp/clips");
        assert_eq!(cfg.max_total_bytes, 1024);
        assert_eq!(cfg.max_age_days, 2);
    }

    #[test]
    fn a_missing_storage_section_is_an_error_not_a_silent_default() {
        // Defaulting `max_total_bytes` to 0 would make every clip "over the cap", which is
        // a far worse failure than refusing to start.
        let err = StorageConfig::from_toml("[buffer]\npre_seconds = 30\n").unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("storage"), "the message must name the section: {}", err.message);
    }

    #[test]
    fn a_missing_config_file_falls_back_to_the_example() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = StorageConfig::load(&dir.path().join("config.toml")).unwrap();
        assert_eq!(cfg, StorageConfig::example().unwrap());
    }

    #[test]
    fn a_malformed_config_file_is_reported_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[storage\nclips_dir = ").unwrap();

        let err = StorageConfig::load(&path).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("[storage]"), "got: {}", err.message);
    }

    #[test]
    fn the_recording_settings_fall_back_to_the_example_config() {
        // A fresh install has no `config.toml`: a first recording must behave exactly as
        // `config.example.toml` documents, which is also what the CLI does.
        let dir = tempfile::tempdir().unwrap();
        let cfg = RecordingConfig::load(&dir.path().join("config.toml")).unwrap();

        assert_eq!(cfg.buffer.pre_seconds, 30);
        assert_eq!(cfg.buffer.post_seconds, 5);
        assert_eq!(cfg.encode.fps, 60, "the example's capture rate");
        assert_eq!(cfg.encode.codec, "h264");
        assert_eq!(cfg.storage.max_total_bytes, 53_687_091_200);
    }

    #[test]
    fn the_recording_settings_come_from_the_same_file_the_review_pane_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[buffer]\npre_seconds = 12\npost_seconds = 3\nsegment_time = 1\n\
             scratch_cap_bytes = 1024\nscratch_dir = \"\"\n\n\
             [encode]\nvendor = \"nvenc\"\ncodec = \"hevc\"\nbitrate_kbps = 20000\n\
             fps = 60\noutput_size = \"1920x1080\"\n\n\
             [storage]\nclips_dir = \"\"\nmax_total_bytes = 2048\nmax_age_days = 1\n",
        )
        .unwrap();

        let recording = RecordingConfig::load(&path).unwrap();
        assert_eq!(recording.buffer.pre_seconds, 12);
        assert_eq!(recording.encode.vendor, "nvenc");
        assert_eq!(recording.storage.max_total_bytes, 2048);

        // The same file, read by the review pane's own type: one file, two readers, and
        // they agree about where clips go.
        let review = StorageConfig::load(&path).unwrap();
        assert_eq!(review.max_total_bytes, recording.storage.max_total_bytes);
    }

    #[test]
    fn a_file_without_the_capture_sections_fails_recording_but_not_the_shell() {
        // `a_capture_only_config_file_does_not_stop_the_shell_from_opening` (lib.rs) is the
        // other half of this: the review pane opens on this file, and only a *recording*
        // refuses it, naming what is missing.
        let err = RecordingConfig::from_toml(
            "[storage]\nclips_dir = \"\"\nmax_total_bytes = 1\nmax_age_days = 1\n",
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("[buffer]"), "the message must name the sections: {}", err.message);
    }

    #[test]
    fn the_app_data_directory_ends_in_localplay() {
        // Whatever the host provides, the two binaries must land in the same subdirectory.
        assert_eq!(app_data_dir().file_name().unwrap(), "localplay");
    }

    // -- the [hotkeys] / [app] half -------------------------------------------------------

    #[test]
    fn the_background_settings_fall_back_to_the_example_config() {
        // A fresh install has no `config.toml`: the hotkey must still be the documented
        // Ctrl+F8 and start-with-system must still be off.
        let dir = tempfile::tempdir().unwrap();
        let cfg = BackgroundConfig::load(&dir.path().join("config.toml")).unwrap();

        assert_eq!(cfg, BackgroundConfig::example().unwrap());
        assert_eq!(cfg.clip_hotkey, DEFAULT_CLIP_HOTKEY);
        assert!(!cfg.start_with_system, "start_with_system is off unless it is asked for");
    }

    #[test]
    fn the_example_config_carries_the_documented_hotkey_and_the_off_default() {
        // `config.example.toml` is what a fresh install *does*, so the fallback constant
        // above and the example must not drift apart. (This is the recursion-free way to
        // pin them: the constant is the fallback for a file with no [hotkeys] at all.)
        let example = BackgroundConfig::example().expect("config.example.toml must stay readable");
        assert_eq!(example.clip_hotkey, DEFAULT_CLIP_HOTKEY);
        assert!(!example.start_with_system, "the example must not autostart by default");

        // And the file really carries the section, rather than the parse defaulting it: a
        // user reading the example has to be able to find the switch it mentions.
        assert!(
            EXAMPLE_CONFIG.contains("[app]") && EXAMPLE_CONFIG.contains("start_with_system = false"),
            "config.example.toml must document [app] start_with_system = false"
        );
    }

    #[test]
    fn a_file_without_the_hotkey_section_still_gets_the_default_chord() {
        // The CLI would reject this file; the window must not. The chord it gets is the
        // documented one, not an empty string that would fail to parse later.
        let cfg = BackgroundConfig::from_toml(
            "[storage]\nclips_dir = \"\"\nmax_total_bytes = 1\nmax_age_days = 1\n",
        )
        .unwrap();

        assert_eq!(cfg.clip_hotkey, DEFAULT_CLIP_HOTKEY);
        assert!(!cfg.start_with_system);
    }

    #[test]
    fn reads_the_hotkey_and_the_app_section_when_the_file_sets_them() {
        let cfg = BackgroundConfig::from_toml(
            "[hotkeys]\nclip = \"Alt+F9\"\n\n[app]\nstart_with_system = true\n",
        )
        .unwrap();

        assert_eq!(cfg.clip_hotkey, "Alt+F9", "the user's chord is used verbatim");
        assert!(cfg.start_with_system);
    }

    #[test]
    fn a_malformed_hotkey_section_is_reported_rather_than_defaulted() {
        // `clip = 8` is a typo a user will make. Falling back to Ctrl+F8 here would leave
        // them pressing a key that is not the one they configured.
        let err = BackgroundConfig::from_toml("[hotkeys]\nclip = 8\n").unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(
            err.message.contains("[hotkeys]"),
            "the message must name the section: {}",
            err.message
        );
    }

    #[test]
    fn a_malformed_config_file_is_reported_with_its_path_for_the_hotkey_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[hotkeys\nclip = ").unwrap();

        let err = BackgroundConfig::load(&path).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);

        // And the recorder's own reader agrees that the file is broken: one file, one
        // verdict, whichever part of the shell reads it.
        assert!(StorageConfig::load(&path).is_err());
    }

    #[test]
    fn a_malformed_app_section_is_reported_rather_than_defaulted() {
        let err = BackgroundConfig::from_toml("[app]\nstart_with_system = \"yes\"\n").unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }
}
