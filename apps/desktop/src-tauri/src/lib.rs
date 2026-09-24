//! The Tauri shell: state, wiring, and the `#[tauri::command]` wrappers that delegate to
//! [`commands`].
//!
//! There is deliberately no *logic* in this file. Each wrapper resolves the application
//! state and hands the call to a plain function in `commands.rs`; the background half — the
//! tray, the hotkey thread and the window's close — is *wired* here but *decided* in
//! [`background`]. This file's job is to be the one place that knows Tauri's types, and to
//! be obviously correct while doing it.
//!
//! That split is what lets the command layer and the background logic be tested without a
//! Tauri runtime and without a window (the test modules at the bottom of `commands.rs` and
//! `background.rs`, and spec §12 on why this project does not put a GUI on the critical path
//! of its tests). What it does **not** do is test the wiring below: the tray really
//! appearing, the window really hiding and a key really arriving all need a desktop, and
//! `docs/verification-status.md` §8 lists them item by item rather than implying otherwise.

pub mod background;
pub mod commands;
pub mod config;

use background::{
    AppStatus, CloseAction, HotkeyStatus, MenuAction, Shell, TrayRendering, TrayState, TrayView,
};
use commands::{
    AppPaths, ClipDto, CommandError, DeleteOutcome, DeleteSessionOutcome, Deps, ErrorCode,
    RecorderHost, RecordedClipDto, RecordingStatusDto, SessionDto, SessionEventDto,
    StorageConfigView, StorageStats, ThumbnailRef,
};
use config::{BackgroundConfig, RecordingConfig, StorageConfig};
use localplay_media::FfmpegBinaries;
use localplay_store::Store;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{MenuBuilder, MenuItem, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, State, WindowEvent, Wry};

/// The one window this application has (`tauri.conf.json` declares it).
const MAIN_WINDOW: &str = "main";

/// How often the tray re-derives itself from the recorder.
///
/// The same half-second the window polls at: the counters it reads are atomics, and the
/// tray only touches the OS when what it would show has actually changed.
const TRAY_POLL: Duration = Duration::from_millis(500);

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
    /// What installing the clip hotkey concluded at startup: the chord, whether a listener
    /// is really there, and why not when it is not. The window shows this instead of
    /// assuming the hotkey works — a hotkey that silently does nothing is the worst outcome
    /// this feature has (`docs/verification-status.md`).
    hotkey: HotkeyStatus,
}

impl AppState {
    /// Open the index and resolve the paths, creating what is missing.
    ///
    /// This is the one step that fails hard. An index that cannot be opened is an index
    /// that can neither list nor delete anything, which leaves the shell with nothing to
    /// show; the caller logs the reason and stops, the same way the CLI refuses to start
    /// when it cannot open its own index.
    pub fn open(
        app_data_dir: &Path,
        storage: StorageConfig,
        hotkey: HotkeyStatus,
    ) -> Result<Self, CommandError> {
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
        Ok(Self { store: Mutex::new(store), bins, paths, storage, warnings, recorder, hotkey })
    }

    /// The recording engine this window drives. No UI is built unless it can record.
    pub fn recorder(&self) -> &RecorderHost {
        &self.recorder
    }

    /// What installing the clip hotkey concluded at startup.
    pub fn hotkey(&self) -> &HotkeyStatus {
        &self.hotkey
    }

    /// The `config.toml` this process read — the file the tray's "open config file" item
    /// reveals and the window names, because this project's configuration story is a file
    /// in a directory rather than a settings dialog.
    pub fn config_path(&self) -> PathBuf {
        self.recorder.config_path()
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

// -- sessions ----------------------------------------------------------------------------
//
// Phase 5's session recorder: one scratch directory per recording, concatenated when it
// stops. The timeline these commands read is computed by the store from the session's media
// epoch — see `commands::session_events` for why it must not be recomputed here.

/// Every session the index knows, newest first.
#[tauri::command(rename_all = "snake_case")]
fn list_sessions(state: State<'_, AppState>) -> Result<Vec<SessionDto>, CommandError> {
    state.with_deps(commands::list_sessions)
}

/// One session's row, for the detail panel.
#[tauri::command(rename_all = "snake_case")]
fn session_detail(
    state: State<'_, AppState>,
    session_id: i64,
) -> Result<SessionDto, CommandError> {
    state.with_deps(|deps| commands::session_detail(deps, session_id))
}

/// A session's timeline markers, in media-time order.
#[tauri::command(rename_all = "snake_case")]
fn session_events(
    state: State<'_, AppState>,
    session_id: i64,
) -> Result<Vec<SessionEventDto>, CommandError> {
    state.with_deps(|deps| commands::session_events(deps, session_id))
}

/// Mark a session as a favourite, or clear it.
#[tauri::command(rename_all = "snake_case")]
fn set_session_favourite(
    state: State<'_, AppState>,
    session_id: i64,
    favourite: bool,
) -> Result<SessionDto, CommandError> {
    state.with_deps(|deps| commands::set_session_favourite(deps, session_id, favourite))
}

/// Delete a session: the row first, then its segments and its recorded file.
#[tauri::command(rename_all = "snake_case")]
fn delete_session(
    state: State<'_, AppState>,
    session_id: i64,
) -> Result<DeleteSessionOutcome, CommandError> {
    state.with_deps(|deps| commands::delete_session(deps, session_id))
}

/// Cut a clip out of a finished session, losslessly. Refused while it is still recording.
#[tauri::command(rename_all = "snake_case")]
fn extract_clip(
    state: State<'_, AppState>,
    session_id: i64,
    start_ms: u64,
    end_ms: u64,
) -> Result<ClipDto, CommandError> {
    state.with_deps(|deps| commands::extract_clip(deps, session_id, start_ms, end_ms))
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

/// What the window shows about the half of the shell that is not a window: the clip hotkey,
/// where the configuration was read from, and what closing the window does.
///
/// It exists so that the two things a user cannot otherwise discover — *which chord to
/// press* and *whether that chord is actually installed* — are on screen, and so that "the
/// config is a file" is a path they can read rather than a guess.
#[tauri::command(rename_all = "snake_case")]
fn app_status(state: State<'_, AppState>) -> Result<AppStatus, CommandError> {
    let config_path = state.config_path();
    Ok(AppStatus {
        hotkey: state.hotkey().clone(),
        config_exists: config_path.is_file(),
        config_path: config_path.to_string_lossy().into_owned(),
        close_hint: background::CLOSE_HINT.to_string(),
    })
}

/// Start the desktop application.
///
/// Nothing in this crate calls this in a test: it opens a window, and neither this
/// development host nor the test suite has a display for one. Everything it wires together
/// is verified through the command layer, `background`'s pure functions and its own tests
/// instead — see the module docs of `background.rs` for exactly what that does and does not
/// prove.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_tracing();

    let app_data_dir = config::app_data_dir();
    let config_path = app_data_dir.join("config.toml");
    let storage = match StorageConfig::load(&config_path) {
        Ok(storage) => storage,
        Err(err) => {
            tracing::error!("{err}");
            eprintln!("localplay: {err}");
            std::process::exit(1);
        }
    };

    // The config story, said once at startup the way the CLI says it: which file, and
    // whether it was there at all. A user who has just edited `[hotkeys] clip` reads the
    // path here rather than wondering which copy of the file the app found.
    let background_cfg = BackgroundConfig::load(&config_path);
    if config_path.is_file() {
        tracing::info!("config: {}", config_path.display());
    } else {
        tracing::info!(
            "config: {} does not exist; the values in config.example.toml are in force",
            config_path.display()
        );
    }

    // The hotkey, installed before anything can want to trigger it. The *listener* is
    // registered here; the thread that turns a press into a clip is started in `setup`,
    // because it needs the application handle to reach the recorder.
    let (hotkey_status, presses) = match &background_cfg {
        Ok(cfg) => background::install_hotkey(&cfg.clip_hotkey),
        Err(err) => (
            HotkeyStatus::not_installed(
                "(none)",
                format!("the config file could not be read, so no hotkey is installed: {}", err.message),
            ),
            None,
        ),
    };
    match &hotkey_status.error {
        // Loud on purpose. This is the failure that would otherwise be invisible until a
        // user pressed the key in a game and nothing happened.
        Some(error) => tracing::error!("the clip hotkey is NOT installed — {error}"),
        None => tracing::info!("the clip hotkey {} is installed", hotkey_status.chord),
    }

    let state = match AppState::open(&app_data_dir, storage, hotkey_status.clone()) {
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

            // The start-with-Windows entry, if the config asks for it. Best effort and
            // never fatal: an autostart entry that could not be written must not stop a
            // recording session. Windows only; elsewhere it reports why it did nothing.
            if let Ok(cfg) = &background_cfg {
                match std::env::current_exe() {
                    Ok(exe) => match background::apply_autostart(cfg.start_with_system, &exe) {
                        Ok(outcome) => tracing::info!(
                            "start_with_system = {}: {}",
                            cfg.start_with_system,
                            outcome.describe()
                        ),
                        Err(err) => tracing::warn!("autostart was not applied: {err}"),
                    },
                    Err(err) => tracing::warn!("autostart was not applied: {err}"),
                }
            }

            if let Err(err) = wire_background(app.handle().clone(), hotkey_status.clone(), presses) {
                // A shell with no tray and no hotkey is still a review window, so this is a
                // warning rather than a refusal to start — but it is never silent.
                tracing::error!("the tray and the hotkey thread were not installed: {err}");
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_clips,
            storage_stats,
            set_favourite,
            trim_clip,
            thumbnail,
            delete_clip,
            list_sessions,
            session_detail,
            session_events,
            set_session_favourite,
            delete_session,
            extract_clip,
            start_recording,
            stop_recording,
            recording_status,
            clip_now,
            app_status
        ])
        .run(tauri::generate_context!())
        .expect("error while running the localplay desktop shell");
}

/// Install the tray and start the two threads that keep it (and the hotkey) live.
///
/// Called once, from `setup`. Split out of `run` so that the wiring is one screen long and
/// the decisions it makes are all `background`'s.
fn wire_background(
    app: AppHandle,
    hotkey: HotkeyStatus,
    presses: Option<std::sync::mpsc::Receiver<()>>,
) -> tauri::Result<()> {
    let shell = AppShell { app };
    let icons = TrayIcons::load()?;
    let wiring = Arc::new(TrayWiring::install(&shell, &hotkey, icons)?);
    // The menu callback needs the wiring, and a Tauri menu callback only gets the handle:
    // so the wiring is managed too, and looked up by type. (`AppState` was managed by the
    // caller, before any of this.)
    shell.app.manage(Arc::clone(&wiring));

    // The tray follows the recorder on its own: a hotkey press, a tray action, and the
    // engine stopping by itself all have to move the icon, and none of them goes through
    // the webview. This thread is the process's lifetime; it ends when the process does.
    {
        let shell = shell.clone();
        let wiring = Arc::clone(&wiring);
        let hotkey = hotkey.clone();
        std::thread::Builder::new()
            .name("localplay-tray".into())
            .spawn(move || loop {
                std::thread::sleep(TRAY_POLL);
                wiring.refresh(&shell, &hotkey);
            })?;
    }

    // The press loop. `on_hotkey_press` is the single trigger: one press, one
    // `RecorderHost::clip_now`, the same call the window's Save clip button makes.
    if let Some(presses) = presses {
        let shell = shell.clone();
        let wiring = Arc::clone(&wiring);
        let hotkey = hotkey.clone();
        std::thread::Builder::new()
            .name("localplay-hotkey-press".into())
            .spawn(move || {
                // `recv` blocks until a press or the channel closes (which happens when the
                // listener thread ends — at process exit).
                while presses.recv().is_ok() {
                    let outcome = background::on_hotkey_press(|| shell.clip_now());
                    if outcome.saved_a_clip() {
                        tracing::info!("{} pressed: {}", hotkey.chord, outcome.describe());
                    } else {
                        tracing::warn!("{} pressed: {}", hotkey.chord, outcome.describe());
                    }
                    wiring.set_last_press(outcome.describe());
                    wiring.refresh(&shell, &hotkey);
                }
            })?;
    }

    // The window's close, which hides rather than quits (`background::close_action` — the
    // rule that keeps a recording alive when a user tidies their screen).
    if let Some(window) = shell.window() {
        let shell = shell.clone();
        let wiring = Arc::clone(&wiring);
        let hotkey = hotkey.clone();
        window.on_window_event(move |event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if background::close_action() == CloseAction::Hide {
                    api.prevent_close();
                    if let Err(err) = shell.hide_window() {
                        tracing::warn!("the window was closed but could not be hidden: {err}");
                    } else {
                        tracing::info!(
                            "the window is hidden: recording continues, and localplay stays in \
                             the tray. Quit from the tray menu (there is no other way out)."
                        );
                    }
                    wiring.refresh(&shell, &hotkey);
                }
            }
        });
    }

    wiring.refresh(&shell, &hotkey);
    Ok(())
}

// ---------------------------------------------------------------------------------------
// The Tauri half of `background`: the window, the recorder, the tray and the file manager.
// Nothing here decides anything — every branch it takes is a value from `background`.
// ---------------------------------------------------------------------------------------

/// The real [`Shell`]: the window, the recorder and the platform's file manager.
#[derive(Clone)]
struct AppShell {
    app: AppHandle,
}

impl AppShell {
    fn window(&self) -> Option<tauri::WebviewWindow> {
        self.app.get_webview_window(MAIN_WINDOW)
    }

    /// The application state, or the reason there is none.
    ///
    /// `try_state` rather than `state`: the state is managed during `setup`, and a tray
    /// event that somehow arrives before that must be reported, not panic a window process.
    fn state(&self) -> Result<State<'_, AppState>, CommandError> {
        self.app.try_state::<AppState>().ok_or_else(|| {
            CommandError::new(
                ErrorCode::Recording,
                "the application state is not available yet",
            )
        })
    }

    /// Whether `config.toml` exists — the tray's "open config" label says which it opens.
    fn config_exists(&self) -> bool {
        self.app.try_state::<AppState>().map(|state| state.config_path().is_file()).unwrap_or(false)
    }
}

impl Shell for AppShell {
    fn window_visible(&self) -> Result<bool, String> {
        match self.window() {
            Some(window) => window.is_visible().map_err(|err| err.to_string()),
            None => Ok(false),
        }
    }

    fn show_window(&self) -> Result<(), String> {
        let window = self.window().ok_or_else(|| "the window is gone".to_string())?;
        // Unminimise first: a window restored from the taskbar is minimised as often as it
        // is hidden, and `show` alone would leave it in the tray area it was restored from.
        let _ = window.unminimize();
        window.show().and_then(|()| window.set_focus()).map_err(|err| err.to_string())
    }

    fn hide_window(&self) -> Result<(), String> {
        self.window()
            .ok_or_else(|| "the window is gone".to_string())?
            .hide()
            .map_err(|err| err.to_string())
    }

    fn status(&self) -> Result<RecordingStatusDto, CommandError> {
        self.state()?.recorder().status()
    }

    fn start_recording(&self) -> Result<RecordingStatusDto, CommandError> {
        // Exactly what the window's Start button does: re-read `config.toml`, then start
        // (`RecorderHost::start_from_config`).
        self.state()?.recorder().start_from_config()
    }

    fn stop_recording(&self) -> Result<RecordingStatusDto, CommandError> {
        self.state()?.recorder().stop()
    }

    fn clip_now(&self) -> Result<RecordedClipDto, CommandError> {
        self.state()?.recorder().clip_now()
    }

    fn open_config(&self) -> Result<PathBuf, String> {
        let state = self
            .app
            .try_state::<AppState>()
            .ok_or_else(|| "the application state is not available yet".to_string())?;
        let path = state.config_path();
        let (program, args) = background::reveal_command(&path, background::FileManager::current());
        std::process::Command::new(&program)
            .args(&args)
            .spawn()
            .map_err(|err| format!("could not run {program}: {err}"))?;
        Ok(path)
    }

    fn quit(&self) -> Result<(), String> {
        // `exit` rather than `close`: every window is destroyed, the process ends, and the
        // recorder was already flushed by `background::dispatch`'s Quit arm.
        self.app.exit(0);
        Ok(())
    }
}

/// The three tray icons, decoded once at startup.
///
/// Generated from the application icon (`icons/icon.png`) with the background keyed out and
/// a state badge added; `cargo test` decodes them, so a corrupt or missing file fails a test
/// rather than a tray.
struct TrayIcons {
    idle: Image<'static>,
    recording: Image<'static>,
    attention: Image<'static>,
}

impl TrayIcons {
    fn load() -> tauri::Result<Self> {
        Ok(Self {
            idle: Image::from_bytes(include_bytes!("../icons/tray-idle.png"))?,
            recording: Image::from_bytes(include_bytes!("../icons/tray-recording.png"))?,
            attention: Image::from_bytes(include_bytes!("../icons/tray-attention.png"))?,
        })
    }

    fn for_state(&self, state: TrayState) -> Image<'static> {
        match state {
            TrayState::Idle => self.idle.clone(),
            TrayState::Recording => self.recording.clone(),
            TrayState::Attention => self.attention.clone(),
        }
    }
}

/// The tray and the menu items whose labels follow the state.
///
/// `rendering` is the whole visible surface as one comparable value
/// ([`TrayView::rendering`]), so "did anything change?" is an equality check and the OS is
/// only touched when something really moved — this runs every half second.
struct TrayWiring {
    tray: tauri::tray::TrayIcon<Wry>,
    window_item: MenuItem<Wry>,
    recording_item: MenuItem<Wry>,
    clip_item: MenuItem<Wry>,
    config_item: MenuItem<Wry>,
    icons: TrayIcons,
    rendering: Mutex<TrayRendering>,
    /// One short line about the last hotkey press, for the tooltip. This is how a user who
    /// is in a game with the window hidden learns that the press did nothing.
    last_press: Mutex<Option<String>>,
}

impl TrayWiring {
    fn install(shell: &AppShell, hotkey: &HotkeyStatus, icons: TrayIcons) -> tauri::Result<Self> {
        let view = TrayView {
            status: &RecordingStatusDto::from(localplay_recorder::RecorderStatus::stopped()),
            hotkey,
            window_visible: true,
            config_exists: shell.config_exists(),
            last_press: None,
        };

        let mut builder = MenuBuilder::new(&shell.app);
        let mut items: Vec<MenuItem<Wry>> = Vec::new();
        for action in MenuAction::ALL {
            if action.separator_before() {
                builder = builder.separator();
            }
            let item = MenuItemBuilder::with_id(action.id(), action.label(&view))
                .enabled(action.enabled(&view))
                .build(&shell.app)?;
            builder = builder.item(&item);
            items.push(item);
        }
        let menu = builder.build()?;

        let tray = TrayIconBuilder::with_id("localplay")
            .icon(icons.for_state(view.tray_state()))
            .tooltip(view.tooltip())
            .menu(&menu)
            // Left click opens the menu (the Windows convention), which is where "Show the
            // window" lives. There is no separate click handler to get out of step with it.
            .show_menu_on_left_click(true)
            .on_menu_event(move |app, event| {
                let shell = AppShell { app: app.clone() };
                let id = event.id().as_ref().to_string();
                let Some(action) = MenuAction::from_id(&id) else {
                    tracing::warn!("the tray sent an id this build did not create: {id}");
                    return;
                };
                let outcome = background::dispatch(action, &shell);
                tracing::info!("tray: {id} → {}", outcome.describe());
                // The wiring's own refresh, from the callback: the poll thread would catch
                // up within half a second, but a menu whose label is already stale when it
                // reopens is a menu that lies twice.
                if let Some(wiring) = app.try_state::<Arc<TrayWiring>>() {
                    if let Some(state) = app.try_state::<AppState>() {
                        wiring.refresh(&shell, state.hotkey());
                    }
                }
            })
            .build(&shell.app)?;

        let find = |action: MenuAction| {
            items
                .iter()
                .find(|item| item.id().as_ref() == action.id())
                .expect("every action was built above")
                .clone()
        };

        Ok(Self {
            tray,
            window_item: find(MenuAction::ShowHideWindow),
            recording_item: find(MenuAction::ToggleRecording),
            clip_item: find(MenuAction::SaveClip),
            config_item: find(MenuAction::OpenConfig),
            icons,
            rendering: Mutex::new(view.rendering()),
            last_press: Mutex::new(None),
        })
    }

    fn set_last_press(&self, line: String) {
        *self.last_press.lock().unwrap_or_else(|err| err.into_inner()) = Some(line);
    }

    /// Re-derive the tray from the shell and apply what changed.
    fn refresh(&self, shell: &AppShell, hotkey: &HotkeyStatus) {
        let status = match shell.status() {
            Ok(status) => status,
            Err(err) => {
                tracing::warn!("the tray could not read the recorder's status: {}", err.message);
                return;
            }
        };
        let last_press = self.last_press.lock().unwrap_or_else(|err| err.into_inner()).clone();
        let view = TrayView {
            status: &status,
            hotkey,
            window_visible: shell.window_visible().unwrap_or(true),
            config_exists: shell.config_exists(),
            last_press: last_press.as_deref(),
        };
        let wanted = view.rendering();

        let mut current = self.rendering.lock().unwrap_or_else(|err| err.into_inner());
        if *current == wanted {
            return;
        }
        if current.state != wanted.state {
            if let Err(err) = self.tray.set_icon(Some(self.icons.for_state(wanted.state))) {
                tracing::warn!("the tray icon could not be changed: {err}");
            }
        }
        if current.tooltip != wanted.tooltip {
            if let Err(err) = self.tray.set_tooltip(Some(wanted.tooltip.clone())) {
                tracing::warn!("the tray tooltip could not be changed: {err}");
            }
        }
        for (item, label) in [
            (&self.window_item, &wanted.window_label),
            (&self.recording_item, &wanted.recording_label),
            (&self.clip_item, &wanted.clip_label),
            (&self.config_item, &wanted.config_label),
        ] {
            if let Err(err) = item.set_text(label) {
                tracing::warn!("a tray menu label could not be changed: {err}");
            }
        }
        if let Err(err) = self.clip_item.set_enabled(wanted.clip_enabled) {
            tracing::warn!("the tray's clip item could not be enabled or disabled: {err}");
        }
        *current = wanted;
    }
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

    /// The hotkey status a test's shell gets: the documented chord, and the truth about
    /// *this* build (`install_hotkey` asks `hotkey::supported`, so off Windows the tests
    /// exercise the "nothing is listening, and here is why" path rather than a chord that
    /// could never fire). No test registers a real chord: that needs Windows.
    fn hotkey_status() -> HotkeyStatus {
        background::install_hotkey(crate::config::DEFAULT_CLIP_HOTKEY).0
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

    /// The tray icons, decoded — they are committed binary assets, and a corrupt one is a
    /// tray with no icon (or a build that fails on someone else's machine). No runtime is
    /// involved: this is the same decode the tray does at startup.
    #[test]
    fn the_tray_icons_are_decodable_pngs_of_one_size() {
        let icons = TrayIcons::load().expect("the embedded tray icons must decode");

        for (state, image) in [
            ("idle", &icons.idle),
            ("recording", &icons.recording),
            ("attention", &icons.attention),
        ] {
            assert_eq!((image.width(), image.height()), (32, 32), "the {state} icon is 32x32");
            assert_eq!(
                image.rgba().len(),
                32 * 32 * 4,
                "the {state} icon is RGBA, four bytes to a pixel"
            );
            let transparent = image.rgba().chunks_exact(4).filter(|px| px[3] == 0).count();
            assert!(
                transparent > 500,
                "the {state} icon must keep the app icon's keyed-out background, not sit on \
                 the taskbar as an opaque block ({transparent} transparent pixels)"
            );
        }

        // The state badge is the whole point: three identical icons would make "recording"
        // a claim the taskbar cannot back up.
        assert_ne!(icons.idle.rgba(), icons.recording.rgba(), "idle and recording differ");
        assert_ne!(
            icons.recording.rgba(),
            icons.attention.rgba(),
            "recording and attention differ"
        );
    }

    /// The config story, from the state a command would answer from: the path the window
    /// shows and the tray's "open config" item reveals is the one this process would read.
    #[test]
    fn the_state_names_the_config_file_this_process_reads() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("localplay");
        let state = AppState::open(&app_dir, storage(), hotkey_status()).unwrap();

        assert_eq!(state.config_path(), app_dir.join("config.toml"));
        assert!(!state.config_path().is_file(), "nothing has written it in this test");
        assert!(state.asset_roots().contains(&state.paths.clips_dir));
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

        let state = AppState::open(&app_dir, storage(), hotkey_status()).unwrap();

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

        let first = AppState::open(&app_dir, storage(), hotkey_status()).unwrap();
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

        let second = AppState::open(&app_dir, storage(), hotkey_status()).unwrap();
        let clips = second.with_deps(commands::list_clips).unwrap();
        assert_eq!(clips.len(), 1, "the second open reads what the first wrote");
        assert_eq!(clips[0].id, id);
    }

    #[test]
    fn the_asset_roots_are_the_clips_directory_and_the_thumbnail_cache() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("localplay");
        let state = AppState::open(&app_dir, storage(), hotkey_status()).unwrap();

        assert_eq!(state.asset_roots(), vec![app_dir.join("clips"), app_dir.join("thumbnails")]);
    }

    #[test]
    fn a_configured_clips_directory_is_used_and_added_to_the_asset_scope() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("localplay");
        let elsewhere = dir.path().join("on-another-drive").join("clips");
        let configured =
            StorageConfig { clips_dir: elsewhere.to_string_lossy().into_owned(), ..storage() };

        let state = AppState::open(&app_dir, configured, hotkey_status()).unwrap();

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
        let state = AppState::open(&app_dir, storage, hotkey_status()).unwrap();

        assert_eq!(state.with_deps(commands::storage_stats).unwrap().cap_bytes, 2048);
    }
}
