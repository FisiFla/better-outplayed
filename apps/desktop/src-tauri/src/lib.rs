//! The Tauri shell: state, wiring, and the `#[tauri::command]` wrappers that delegate to
//! [`commands`].
//!
//! There is deliberately no logic in this file. Each wrapper resolves the application
//! state and hands the call to a plain function in `commands.rs`, which is what lets the
//! whole command layer be tested without a Tauri runtime and without a window (see the
//! test module at the bottom of `commands.rs`, and spec §12 on why this project does not
//! put a GUI on the critical path of its tests).

pub mod commands;
pub mod config;

use commands::{
    AppPaths, ClipDto, CommandError, DeleteOutcome, Deps, ErrorCode, RecorderHost,
    RecordedClipDto, RecordingStatusDto, StorageConfigView, StorageStats, ThumbnailRef,
};
use config::{RecordingConfig, StorageConfig};
use localplay_media::FfmpegBinaries;
use localplay_store::Store;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::{Manager, State};

/// Everything the commands run against, resolved once at startup.
pub struct AppState {
    /// The SQLite index the CLI writes and this shell reads (spec §5.5). Behind a `Mutex`
    /// because `rusqlite::Connection` is `Send` but not `Sync`, and Tauri commands can be
    /// invoked from more than one thread.
    store: Mutex<Store>,
    /// `None` when ffmpeg could not be found: the list and the storage panel still work,
    /// and the two commands that need ffmpeg report why they cannot.
    bins: Option<FfmpegBinaries>,
    paths: AppPaths,
    storage: StorageConfig,
    warnings: Vec<String>,
    /// The recording this window started, if any — the engine the CLI also drives
    /// (`localplay-recorder`). It is a host rather than a bare handle because a start has
    /// to resolve the application data directory and the ffmpeg sidecar binaries, and
    /// both are already here.
    recorder: RecorderHost,
}

impl AppState {
    /// Open the index and resolve the paths, creating what is missing.
    ///
    /// This is the one step that fails hard. An index that cannot be opened is an index
    /// that can neither list nor delete anything, which leaves the shell with nothing to
    /// show; the caller logs the reason and stops, the same way the CLI refuses to start
    /// when it cannot open its own index.
    pub fn open(app_data_dir: &Path, storage: StorageConfig) -> Result<Self, CommandError> {
        std::fs::create_dir_all(app_data_dir).map_err(|e| {
            CommandError::new(
                ErrorCode::Io,
                format!(
                    "could not create the application data directory {}: {e}",
                    app_data_dir.display()
                ),
            )
        })?;

        let paths = AppPaths::resolve(app_data_dir, &storage.clips_dir);
        std::fs::create_dir_all(&paths.clips_dir).map_err(|e| {
            CommandError::new(
                ErrorCode::Io,
                format!("could not create the clips directory {}: {e}", paths.clips_dir.display()),
            )
        })?;

        let store = Store::open(&paths.db_path).map_err(|e| {
            CommandError::new(
                ErrorCode::Store,
                format!("could not open the clip index {}: {e:#}", paths.db_path.display()),
            )
        })?;
        store.migrate().map_err(|e| {
            CommandError::new(
                ErrorCode::Store,
                format!("could not migrate the clip index {}: {e:#}", paths.db_path.display()),
            )
        })?;

        // ffmpeg is a startup *warning*, not a startup failure: everything this shell is
        // for except trimming and thumbnails works without it, and a missing sidecar is
        // something the user can fix while the app is running. The warning travels to the
        // storage panel, where it is visible, rather than only reaching a log nobody reads.
        let (bins, warnings) = match FfmpegBinaries::discover(None) {
            Ok(bins) => {
                tracing::info!("using ffmpeg at {}", bins.ffmpeg.display());
                (Some(bins), Vec::new())
            }
            Err(err) => {
                tracing::warn!("{err:#}");
                let warning = format!(
                    "ffmpeg was not found, so clips cannot be trimmed or thumbnailed: {err:#}"
                );
                (None, vec![warning])
            }
        };

        tracing::info!(
            "clip index {} (schema v{}): {} clips, {} bytes",
            paths.db_path.display(),
            store.schema_version().unwrap_or(-1),
            store.list_clips().map(|c| c.len()).unwrap_or(0),
            store.total_bytes().unwrap_or(0)
        );

        let recorder = RecorderHost::new(app_data_dir.to_path_buf(), bins.clone());
        Ok(Self { store: Mutex::new(store), bins, paths, storage, warnings, recorder })
    }

    /// The recording engine this window drives. No UI is built unless it can record.
    pub fn recorder(&self) -> &RecorderHost {
        &self.recorder
    }

    /// The paths the asset protocol has to be allowed to serve (spec §9).
    pub fn asset_roots(&self) -> Vec<PathBuf> {
        self.paths.asset_roots().iter().map(|p| p.to_path_buf()).collect()
    }

    /// Build the explicit dependencies for one command call and run it.
    ///
    /// The guard lives for the whole call, so the store cannot be mutated by a concurrent
    /// command halfway through one; no command awaits anything, so holding it is cheap.
    fn with_deps<T>(
        &self,
        run: impl FnOnce(&Deps<'_>) -> Result<T, CommandError>,
    ) -> Result<T, CommandError> {
        let store = self.store.lock().map_err(|_| {
            CommandError::new(
                ErrorCode::Store,
                "the clip index is unusable: an earlier command panicked while holding it",
            )
        })?;
        let storage = StorageConfigView::from(&self.storage);
        let deps = Deps {
            store: &store,
            bins: self.bins.as_ref(),
            paths: &self.paths,
            storage: &storage,
            warnings: &self.warnings,
        };
        run(&deps)
    }
}

// ---------------------------------------------------------------------------------------
// The IPC commands. Every one of these is a delegation: resolve the state, call the plain
// function in `commands.rs`, hand back its result. Nothing is decided here.
// ---------------------------------------------------------------------------------------

/// Every clip in the index, newest first.
#[tauri::command(rename_all = "snake_case")]
fn list_clips(state: State<'_, AppState>) -> Result<Vec<ClipDto>, CommandError> {
    state.with_deps(commands::list_clips)
}

/// Usage, the configured cap and age limit, and the cleanup policy's verdict.
#[tauri::command(rename_all = "snake_case")]
fn storage_stats(state: State<'_, AppState>) -> Result<StorageStats, CommandError> {
    state.with_deps(commands::storage_stats)
}

/// Mark a clip as a favourite, or clear it.
#[tauri::command(rename_all = "snake_case")]
fn set_favourite(
    state: State<'_, AppState>,
    id: i64,
    favourite: bool,
) -> Result<ClipDto, CommandError> {
    state.with_deps(|deps| commands::set_favourite(deps, id, favourite))
}

/// Cut `[start_ms, end_ms)` into a new clip file with a lossless stream copy.
#[tauri::command(rename_all = "snake_case")]
fn trim_clip(
    state: State<'_, AppState>,
    id: i64,
    start_ms: u64,
    end_ms: u64,
) -> Result<ClipDto, CommandError> {
    state.with_deps(|deps| commands::trim_clip(deps, id, start_ms, end_ms))
}

/// A cached JPEG thumbnail for one clip at one timestamp.
#[tauri::command(rename_all = "snake_case")]
fn thumbnail(
    state: State<'_, AppState>,
    id: i64,
    at_ms: u64,
) -> Result<ThumbnailRef, CommandError> {
    state.with_deps(|deps| commands::thumbnail(deps, id, at_ms))
}

/// Delete a clip: the row first, then the file (spec §8.2).
#[tauri::command(rename_all = "snake_case")]
fn delete_clip(state: State<'_, AppState>, id: i64) -> Result<DeleteOutcome, CommandError> {
    state.with_deps(|deps| commands::delete_clip(deps, id))
}

// -- recording ---------------------------------------------------------------------------
//
// These four are `async`, unlike every other command here, for one reason: tauri runs a
// synchronous command on the webview's own thread, and two of these block for real time —
// `clip_now` waits for the post-roll to be written (seconds), and a start runs the
// encoder smoke test. An `async fn` command runs on the runtime's worker threads instead,
// which is what keeps the window responsive while a clip is being saved.

/// Start recording through the shared engine.
#[tauri::command(rename_all = "snake_case")]
async fn start_recording(state: State<'_, AppState>) -> Result<RecordingStatusDto, CommandError> {
    // Read at start time, not at window-open time: a capture-only mistake must not stop the
    // review pane from opening (see `config.rs`).
    let settings = RecordingConfig::load(&state.recorder.config_path())?;
    state.recorder.start(&settings)
}

/// Stop recording and flush the encoder. Idempotent.
#[tauri::command(rename_all = "snake_case")]
async fn stop_recording(state: State<'_, AppState>) -> Result<RecordingStatusDto, CommandError> {
    state.recorder.stop()
}

/// The live recording status. Answers "not running" before anything has been started.
#[tauri::command(rename_all = "snake_case")]
async fn recording_status(state: State<'_, AppState>) -> Result<RecordingStatusDto, CommandError> {
    state.recorder.status()
}

/// Take a clip now: wait for the post-roll, splice it losslessly, index it.
#[tauri::command(rename_all = "snake_case")]
async fn clip_now(state: State<'_, AppState>) -> Result<RecordedClipDto, CommandError> {
    state.recorder.clip_now()
}

/// Start the desktop application.
///
/// Nothing in this crate calls this in a test: it opens a window, and neither this
/// development host nor the test suite has a display for one. Everything it wires together
/// is verified through the command layer instead.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_tracing();

    let app_data_dir = config::app_data_dir();
    let storage = match StorageConfig::load(&app_data_dir.join("config.toml")) {
        Ok(storage) => storage,
        Err(err) => {
            tracing::error!("{err}");
            eprintln!("localplay: {err}");
            std::process::exit(1);
        }
    };

    let state = match AppState::open(&app_data_dir, storage) {
        Ok(state) => state,
        Err(err) => {
            tracing::error!("{err}");
            eprintln!("localplay: {err}");
            std::process::exit(1);
        }
    };
    let asset_roots = state.asset_roots();

    tauri::Builder::default()
        .setup(move |app| {
            // Playback is served by Tauri's asset protocol, scoped to the clips directory
            // (spec §9) — there is no file server in this application. `tauri.conf.json`
            // carries the static scope for the default layout; this adds the directories
            // this process actually resolved, which is what makes a configured
            // `storage.clips_dir` outside the application data directory playable at all.
            let scope = app.asset_protocol_scope();
            for dir in &asset_roots {
                if let Err(err) = scope.allow_directory(dir, true) {
                    tracing::warn!(
                        "the asset protocol was not scoped to {}: {err}. Clips in that \
                         directory will not play.",
                        dir.display()
                    );
                }
            }
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_clips,
            storage_stats,
            set_favourite,
            trim_clip,
            thumbnail,
            delete_clip,
            start_recording,
            stop_recording,
            recording_status,
            clip_now
        ])
        .run(tauri::generate_context!())
        .expect("error while running the localplay desktop shell");
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    // `try_init` rather than `init`: a second initialisation (a test harness, an embedding
    // process) must not panic the application.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> StorageConfig {
        StorageConfig::example().unwrap()
    }

    /// The directory holding this crate's manifest, i.e. `apps/desktop/src-tauri`: the
    /// place `tauri.conf.json` and the platform configs resolve their relative paths from.
    fn tauri_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn config(file: &str) -> serde_json::Value {
        let path = tauri_dir().join(file);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {file}: {e}"));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("parsing {file}: {e}"))
    }

    /// The half of the packaging contract that `crates/media` cannot check by itself.
    ///
    /// An installed app finds its ffmpeg by looking in `<resource dir>/binaries/`; the
    /// bundler writes each `bundle.resources` entry to `<resource dir>/<destination>`. If
    /// those two ever disagree the installer ships the sidecars to a path nothing reads, the
    /// app falls back to `PATH`, and the failure lands on a user's machine — which is the
    /// defect this test exists to make impossible to ship again. The directory name is read
    /// from the media crate rather than spelled out here, so renaming it there fails here.
    #[test]
    fn the_bundle_maps_the_sidecars_where_discovery_reads_them() {
        let config = config("tauri.conf.json");

        assert_eq!(
            config["bundle"]["active"],
            serde_json::Value::Bool(true),
            "bundling must be on, or there is no distributable artifact at all"
        );

        let resources = config["bundle"]["resources"]
            .as_object()
            .expect("bundle.resources is a path-to-destination map");
        assert_eq!(resources.len(), 1, "exactly one mapping rule, the sidecar directory");
        let (source, destination) = resources.iter().next().unwrap();
        let destination = destination.as_str().expect("the destination is a string");

        assert_eq!(
            destination.trim_end_matches('/'),
            localplay_media::binaries::SIDECAR_DIR_NAME,
            "the bundler must write the sidecars into the directory `FfmpegBinaries::discover` \
             reads: `binaries/` under the resource directory"
        );

        // The source is the directory `cargo xtask sidecars fetch` writes, i.e. the same
        // one `FfmpegBinaries::discover`'s development fallback reads from a checkout.
        assert_eq!(source, "../../../binaries/");
        assert!(
            tauri_dir().join(source).is_dir(),
            "{} must exist for the bundle to be buildable at all: `tauri-build` refuses to \
             compile when a mapped resource path is missing, which is why the directory \
             carries a committed `.gitkeep`",
            tauri_dir().join(source).display()
        );
    }

    /// The metadata the installers are built from, and the icon files they name. Both are
    /// things a bundle silently gets wrong: a non-existent icon path fails the bundling
    /// step, and the placeholder identifier Tauri rejects (`com.tauri.dev`) is one typo away.
    #[test]
    fn the_bundle_metadata_and_icons_are_present_and_plausible() {
        let config = config("tauri.conf.json");
        let bundle = &config["bundle"];

        assert_eq!(config["productName"], "localplay");
        assert_eq!(config["mainBinaryName"], "localplay", "the installed exe is localplay");
        assert_eq!(config["version"], "0.1.0", "in step with the workspace version");

        let identifier = config["identifier"].as_str().expect("identifier");
        assert_ne!(identifier, "com.tauri.dev", "Tauri refuses to bundle the placeholder id");
        assert!(
            identifier.matches('.').count() >= 2
                && identifier
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'),
            "identifier must be a reverse-DNS id: {identifier}"
        );

        for key in ["publisher", "copyright", "category", "homepage", "shortDescription"] {
            let value = bundle[key].as_str().unwrap_or_else(|| panic!("bundle.{key} is set"));
            assert!(!value.trim().is_empty(), "bundle.{key} is empty");
        }
        assert_eq!(bundle["publisher"], "FisiFla", "the copyright holder in LICENSE-MIT");

        let icons = bundle["icon"].as_array().expect("bundle.icon is a list");
        assert!(!icons.is_empty(), "a bundle needs at least one icon");
        for icon in icons {
            let path = tauri_dir().join(icon.as_str().expect("icon paths are strings"));
            assert!(
                path.is_file(),
                "bundle.icon names {}, which does not exist — the bundling step would fail",
                path.display()
            );
        }
    }

    /// The installer targets, which are the only reason the platform configs exist: a build
    /// must not try to produce another platform's artifact.
    #[test]
    fn every_platform_config_narrows_the_bundle_targets() {
        for (file, expected) in [
            ("tauri.windows.conf.json", vec!["nsis"]),
            ("tauri.macos.conf.json", vec!["app"]),
        ] {
            let config = config(file);
            let targets: Vec<&str> = config["bundle"]["targets"]
                .as_array()
                .unwrap_or_else(|| panic!("{file} sets an explicit list of bundle targets"))
                .iter()
                .map(|t| t.as_str().expect("targets are strings"))
                .collect();
            assert_eq!(targets, expected, "{file}");
        }
    }

    #[test]
    fn opening_the_shell_creates_the_index_and_the_clips_directory() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("localplay");

        let state = AppState::open(&app_dir, storage()).unwrap();

        assert!(app_dir.join("localplay.db").is_file(), "the index is created");
        assert!(app_dir.join("clips").is_dir(), "and so is the clips directory");
        assert!(state.bins.is_some(), "ffmpeg is on PATH on this host");
        assert!(state.warnings.is_empty());

        // The wrappers' shared entry point works against the real state, with no runtime.
        let stats = state.with_deps(commands::storage_stats).unwrap();
        assert_eq!(stats.clip_count, 0);
        assert_eq!(stats.cap_bytes, 53_687_091_200, "the example's 50 GiB cap");
        assert!(state.with_deps(commands::list_clips).unwrap().is_empty());
    }

    #[test]
    fn reopening_the_shell_reuses_the_index_instead_of_recreating_it() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("localplay");

        let first = AppState::open(&app_dir, storage()).unwrap();
        let id = {
            let store = first.store.lock().unwrap();
            store
                .insert_clip(&localplay_store::NewClip {
                    path: app_dir.join("clips").join("kept.mp4"),
                    started_at_ms: 0,
                    duration_ms: 1_000,
                    size_bytes: 1,
                    codec: "h264".to_string(),
                })
                .unwrap()
        };
        drop(first);

        let second = AppState::open(&app_dir, storage()).unwrap();
        let clips = second.with_deps(commands::list_clips).unwrap();
        assert_eq!(clips.len(), 1, "the second open reads what the first wrote");
        assert_eq!(clips[0].id, id);
    }

    #[test]
    fn the_asset_roots_are_the_clips_directory_and_the_thumbnail_cache() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("localplay");
        let state = AppState::open(&app_dir, storage()).unwrap();

        assert_eq!(state.asset_roots(), vec![app_dir.join("clips"), app_dir.join("thumbnails")]);
    }

    #[test]
    fn a_configured_clips_directory_is_used_and_added_to_the_asset_scope() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("localplay");
        let elsewhere = dir.path().join("on-another-drive").join("clips");
        let configured =
            StorageConfig { clips_dir: elsewhere.to_string_lossy().into_owned(), ..storage() };

        let state = AppState::open(&app_dir, configured).unwrap();

        assert!(elsewhere.is_dir(), "the configured directory is created");
        assert_eq!(state.asset_roots()[0], elsewhere, "and it is what playback is scoped to");
        assert_eq!(state.paths.db_path, app_dir.join("localplay.db"), "the index stays put");
    }

    #[test]
    fn a_capture_only_config_file_does_not_stop_the_shell_from_opening() {
        // The shell reads `[storage]` and nothing else (see `config.rs`), so a file with a
        // capture setting the CLI would reject still opens the review UI.
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("localplay");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            "[encode]\nvendor = \"software\"\n\n[storage]\nclips_dir = \"\"\n\
             max_total_bytes = 2048\nmax_age_days = 1\n",
        )
        .unwrap();

        let storage = StorageConfig::load(&app_dir.join("config.toml")).unwrap();
        let state = AppState::open(&app_dir, storage).unwrap();

        assert_eq!(state.with_deps(commands::storage_stats).unwrap().cap_bytes, 2048);
    }
}
