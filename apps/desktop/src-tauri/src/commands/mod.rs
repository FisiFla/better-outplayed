//! The desktop shell's command layer — everything the IPC commands actually do.
//!
//! # Why the logic lives here and not in the `#[tauri::command]` functions
//!
//! Every function in this module is a plain function over explicit dependencies: a
//! [`Store`], an optional [`FfmpegBinaries`], a set of paths, a storage policy. None of
//! them takes a `State`, an `AppHandle` or a window, and none of them is `async`. The
//! `#[tauri::command]` wrappers in `lib.rs` resolve the application state and delegate,
//! and contain no logic of their own.
//!
//! That split is the whole point of this file. It is what lets the suite below drive the
//! real command bodies against a **real** SQLite store and **real** ffmpeg-produced files
//! on disk, with no Tauri runtime and no window — a window this repository's development
//! host has no display for, and which a test suite must never open anyway (spec §12).
//!
//! # What these commands do not do
//!
//! No command here captures the screen, enumerates windows or synthesises input, and none
//! of them ever re-encodes: `trim_clip` is [`localplay_media::edit::trim_lossless`], a
//! stream copy (`-c copy`), and the UI says so. The `events` table is not read — game
//! event markers are Phase 4 and the timeline draws none (see `src/lib/components/
//! Timeline.svelte`).

use localplay_media::FfmpegBinaries;
use localplay_store::Store;
use std::path::{Path, PathBuf};

mod clips;
mod dto;
mod error;
mod recorder;
mod sessions;
mod settings;
#[cfg(test)]
mod test_support;

pub use clips::*;
pub use dto::*;
pub use error::*;
pub use recorder::*;
pub use sessions::*;
pub use settings::*;

/// The app-data-relative directories the shell reads and writes.
///
/// `clips_dir` is where the recorder writes clips and where a trim's new file goes — next
/// to its parent, which is what makes a trimmed clip visible to the CLI's storage policy
/// without any extra wiring. `thumbnails_dir` is a cache: nothing but this app reads it,
/// and losing it costs one ffmpeg run per clip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub clips_dir: PathBuf,
    pub thumbnails_dir: PathBuf,
    pub db_path: PathBuf,
}

impl AppPaths {
    /// Resolve the paths for `app_data_dir` under the `[storage]` settings.
    ///
    /// An empty `clips_dir` means the `clips` directory under the application data
    /// directory — the same rule the CLI applies, which is what makes the two binaries
    /// operate on one library instead of two.
    pub fn resolve(app_data_dir: &Path, clips_dir_setting: &str) -> Self {
        let clips_dir = if clips_dir_setting.is_empty() {
            app_data_dir.join("clips")
        } else {
            PathBuf::from(clips_dir_setting)
        };
        Self {
            clips_dir,
            thumbnails_dir: app_data_dir.join("thumbnails"),
            db_path: app_data_dir.join("localplay.db"),
        }
    }

    /// The directories the asset protocol must be allowed to serve.
    ///
    /// Playback (`<video>`) only ever requests paths under `clips_dir`; the thumbnails
    /// directory is here because a cached JPEG is served to an `<img>` the same way. Both
    /// are handed to the runtime scope in `lib.rs`; the static scope in `tauri.conf.json`
    /// covers the default layout only, and a configured `storage.clips_dir` is exactly the
    /// case that needs the runtime one.
    pub fn asset_roots(&self) -> [&Path; 2] {
        [self.clips_dir.as_path(), self.thumbnails_dir.as_path()]
    }
}

/// Everything a command needs, passed explicitly.
///
/// `bins` is optional because ffmpeg is discovered at startup and a missing ffmpeg must
/// not stop the shell from opening: listing clips and reading the storage policy need no
/// ffmpeg at all, and the two commands that do need it fail with
/// [`ErrorCode::FfmpegUnavailable`] and the discovery error rather than taking the window
/// down with them.
#[derive(Clone, Copy)]
pub struct Deps<'a> {
    pub store: &'a Store,
    pub bins: Option<&'a FfmpegBinaries>,
    pub paths: &'a AppPaths,
    pub storage: &'a StorageConfigView,
    /// Problems found at startup that the UI should surface rather than hide.
    pub warnings: &'a [String],
}

impl<'a> Deps<'a> {
    fn binaries(&self) -> Result<&'a FfmpegBinaries, CommandError> {
        self.bins.ok_or_else(|| {
            CommandError::new(
                ErrorCode::FfmpegUnavailable,
                "ffmpeg was not found, so clips cannot be trimmed or thumbnailed. \
                 Install ffmpeg on PATH or place the sidecar binaries next to the \
                 application.",
            )
        })
    }
}

/// The storage policy as the commands see it (spec §8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageConfigView {
    pub max_total_bytes: u64,
    pub max_age_days: u64,
}

impl From<&crate::config::StorageConfig> for StorageConfigView {
    fn from(cfg: &crate::config::StorageConfig) -> Self {
        Self { max_total_bytes: cfg.max_total_bytes, max_age_days: cfg.max_age_days }
    }
}
