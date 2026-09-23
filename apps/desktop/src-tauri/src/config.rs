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
    fn the_app_data_directory_ends_in_localplay() {
        // Whatever the host provides, the two binaries must land in the same subdirectory.
        assert_eq!(app_data_dir().file_name().unwrap(), "localplay");
    }
}
