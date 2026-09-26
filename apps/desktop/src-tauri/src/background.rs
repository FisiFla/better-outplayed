//! The half of the desktop shell that is not a window: the system tray, the close-to-hide
//! rule, the global clip hotkey and the start-with-Windows entry.
//!
//! # Why this file is mostly plain functions
//!
//! The same reason `commands.rs` is: **this project's test suite never opens a window**. A
//! tray menu is a menu on a real desktop, a hotkey is a message on a real Windows message
//! queue, and closing a window is something a person does with a mouse — none of which a
//! headless run can do. So every decision lives here as a function over plain data
//! ([`MenuAction::label`], [`TrayView::tray_state`], [`dispatch`], [`close_action`],
//! [`sync_autostart`]), and `lib.rs` is left with a thin translation into Tauri calls whose
//! only job is to be obviously correct.
//!
//! What that buys, stated honestly: the *logic* is unit-tested and the wiring compiles for
//! `x86_64-pc-windows-msvc`; that the tray appears, that the X hides the window, and that a
//! real `Ctrl+F8` produces a clip on a real machine is **not** verified by any test in this
//! repository. See `docs/verification-status.md`.
//!
//! # The one hotkey implementation
//!
//! The chord is parsed and registered by `localplay_events::hotkey` — the same module the
//! CLI installs from the same `config.toml` — through [`install_hotkey`]. There is no second
//! hotkey path to drift, and a registration failure (the chord is somebody else's, which
//! includes a second localplay) is returned as data the window and the tray can both show.

use crate::commands::{CommandError, RecordedClipDto, RecordingStatusDto};
use localplay_events::hotkey::{self, Hotkey};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

// ---------------------------------------------------------------------------------------
// The hotkey
// ---------------------------------------------------------------------------------------

/// What the shell can say about the clip hotkey: the chord, whether a listener is installed,
/// and — when it is not — why not, in a sentence a user can act on.
///
/// Mirrored by hand in `src/lib/types.ts`, and pinned by
/// `the_hotkey_status_json_matches_the_typescript_interface` below.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyStatus {
    /// The chord as `config.toml` asked for it, normalised by `Hotkey`'s `Display`
    /// (`Ctrl+F8`). Present even when nothing is listening: it is what the user was told to
    /// press, and a UI that hid it on failure would leave them with no idea what to try.
    pub chord: String,
    /// True only when a listener is really installed in this process.
    pub installed: bool,
    /// Why it is not installed. `None` exactly when `installed` is true.
    pub error: Option<String>,
}

impl HotkeyStatus {
    /// A listener that is installed and holding the chord.
    pub fn installed(chord: impl Into<String>) -> Self {
        Self { chord: chord.into(), installed: true, error: None }
    }

    /// A chord nothing is listening for, and the reason.
    pub fn not_installed(chord: impl Into<String>, error: impl Into<String>) -> Self {
        Self { chord: chord.into(), installed: false, error: Some(error.into()) }
    }
}

/// Install the clip hotkey named by `[hotkeys] clip`, returning what the window should show
/// and the channel a press arrives on (`None` when nothing was installed).
///
/// Three outcomes, all of them *values* rather than a silent nothing:
///
/// * the chord does not parse — a typo in `config.toml`;
/// * this build has no global hotkey at all ([`hotkey::supported`] is false off Windows),
///   which is said out loud rather than showing a chord that cannot fire;
/// * `RegisterHotKey` refused it — the chord is already held, by another application or by
///   another localplay. The error names the chord and the likely owner
///   (`crates/events/src/hotkey.rs` composes it), because this is the failure that would
///   otherwise be invisible.
pub fn install_hotkey(config_value: &str) -> (HotkeyStatus, Option<Receiver<()>>) {
    let parsed = match Hotkey::parse(config_value) {
        Ok(parsed) => parsed,
        Err(err) => {
            return (
                HotkeyStatus::not_installed(
                    config_value,
                    format!(
                        "[hotkeys] clip = \"{config_value}\" could not be read ({err}), so no \
                         hotkey is installed. It wants a chord like \"Ctrl+F8\"."
                    ),
                ),
                None,
            );
        }
    };

    let chord = parsed.to_string();
    if !hotkey::supported() {
        return (
            HotkeyStatus::not_installed(
                chord,
                "a global hotkey is a Windows feature (RegisterHotKey), and this build is \
                 not Windows: nothing is listening. Use the Start recording / Save clip \
                 buttons in this window, or the tray menu."
                    .to_string(),
            ),
            None,
        );
    }

    match hotkey::listen(parsed) {
        Ok(presses) => (HotkeyStatus::installed(chord), Some(presses)),
        Err(err) => (HotkeyStatus::not_installed(chord, err.to_string()), None),
    }
}

/// One hotkey press → **exactly one** clip, through the engine entry point the window's
/// Save clip button and the CLI's driver loop call (`RecorderHost::clip_now`).///
/// `take` is that call in the shipping wiring, and a counting stub in the tests. A
/// `FnOnce` rather than a handle on purpose: one press can ask for one clip and no more, so
/// there is no loop here to double-fire, and no retry to turn one keypress into two files.
pub fn on_hotkey_press(
    take: impl FnOnce() -> Result<RecordedClipDto, CommandError>,
) -> PressOutcome {
    match take() {
        Ok(clip) => PressOutcome::Clip(clip),
        Err(err) => PressOutcome::Failed(err),
    }
}

/// What one hotkey press produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PressOutcome {
    Clip(RecordedClipDto),
    Failed(CommandError),
}

impl PressOutcome {
    /// The one short line the tray tooltip carries, and the log records.
    pub fn describe(&self) -> String {
        match self {
            PressOutcome::Clip(clip) => format!("saved {}", clip_file_name(&clip.path)),
            PressOutcome::Failed(err) => err.message.clone(),
        }
    }

    /// Whether a clip is on disk because of this press.
    pub fn saved_a_clip(&self) -> bool {
        matches!(self, PressOutcome::Clip(_))
    }
}

/// The file name of a path that may use either separator: the clip paths in a DTO are
/// Windows paths even when this code runs on the development host.
fn clip_file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

// ---------------------------------------------------------------------------------------
// The tray
// ---------------------------------------------------------------------------------------

/// Which of the three tray icons, and which tooltip.
///
/// Two very different problems land on `Attention` — the recorder stopped for a failure, or
/// the clip hotkey is not installed — because a 16x16 icon cannot deliver a sentence. The
/// tooltip says which, and the window says why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Idle,
    Recording,
    Attention,
}

/// Everything the tray renders from, as plain data. Built in `lib.rs` from the recorder's
/// status, the hotkey's status and the window's visibility.
pub struct TrayView<'a> {
    pub status: &'a RecordingStatusDto,
    pub hotkey: &'a HotkeyStatus,
    /// Whether the window is on screen right now (the tray's first item toggles it).
    pub window_visible: bool,
    /// Whether `config.toml` exists — the tray's "open config" item says which it opens.
    pub config_exists: bool,
    /// One short line about the last hotkey press, when there has been one. This is how a
    /// user who is in a game with this window hidden learns that the press did nothing.
    pub last_press: Option<&'a str>,
}

impl TrayView<'_> {
    pub fn tray_state(&self) -> TrayState {
        if !self.hotkey.installed || self.status.error.is_some() {
            TrayState::Attention
        } else if self.status.running {
            TrayState::Recording
        } else {
            TrayState::Idle
        }
    }

    /// The hover text. Read by a user who is alt-tabbed out of a game, so it says what is
    /// happening and what the key does.
    pub fn tooltip(&self) -> String {
        let mut text = match self.tray_state() {
            TrayState::Attention if !self.hotkey.installed => format!(
                "localplay — the clip hotkey {} is NOT installed. Open the window for why; \
                 the tray menu still records and saves clips.",
                self.hotkey.chord
            ),
            TrayState::Attention => format!(
                "localplay — the recorder stopped: {}",
                self.status.error.as_deref().unwrap_or("no reason was reported")
            ),
            TrayState::Recording => format!(
                "localplay — recording. Press {} to take a clip.",
                self.hotkey.chord
            ),
            TrayState::Idle => format!(
                "localplay — not recording. Press {} for a clip once recording is on.",
                self.hotkey.chord
            ),
        };
        if let Some(press) = self.last_press {
            text.push_str(&format!(" Last press: {press}."));
        }
        truncate(&text, TOOLTIP_LIMIT)
    }
}

/// The longest tooltip this shell will hand the OS.
///
/// Windows' `NOTIFYICONDATA.szTip` is 128 characters including the terminator, and a tooltip
/// cut off by the shell with no indication is worse than one this code cut with a marker.
pub const TOOLTIP_LIMIT: usize = 120;

/// Cut `text` to `limit` characters on a character boundary, marking that it was cut.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(limit.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// One item of the tray menu: what the user picks, in the words they read.
///
/// The three with a dynamic label (the window toggle, the recording toggle, the config item)
/// are the reason this is a function of [`TrayView`] rather than a constant list: a menu that
/// says "Start recording" while a recording runs is a menu that lies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// Show the window, or hide it when it is already on screen.
    ShowHideWindow,
    /// Start recording, or stop it when one is running.
    ToggleRecording,
    /// Take a clip through the engine, exactly as the Save clip button does.
    SaveClip,
    /// Reveal `config.toml` (or the folder it belongs in) in the file manager.
    OpenConfig,
    /// Stop the recorder, then exit the process.
    Quit,
}

impl MenuAction {
    /// The menu, in order. `Quit` is separated from the rest.
    pub const ALL: [MenuAction; 5] = [
        MenuAction::ShowHideWindow,
        MenuAction::ToggleRecording,
        MenuAction::SaveClip,
        MenuAction::OpenConfig,
        MenuAction::Quit,
    ];

    /// The stable id the menu is built with and the event handler dispatches on. These
    /// strings cross a runtime boundary, so they are pinned by a test.
    pub fn id(self) -> &'static str {
        match self {
            MenuAction::ShowHideWindow => "window",
            MenuAction::ToggleRecording => "recording",
            MenuAction::SaveClip => "clip",
            MenuAction::OpenConfig => "config",
            MenuAction::Quit => "quit",
        }
    }

    /// The action for a menu id, or `None` for an id this build did not create (a menu item
    /// from a newer version, say — not an error, and not something to guess at).
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.id() == id)
    }

    /// The label for this state.
    pub fn label(self, view: &TrayView<'_>) -> String {
        match self {
            MenuAction::ShowHideWindow if view.window_visible => "Hide the window".to_string(),
            MenuAction::ShowHideWindow => "Show the window".to_string(),
            MenuAction::ToggleRecording if view.status.running => "Stop recording".to_string(),
            MenuAction::ToggleRecording => "Start recording".to_string(),
            MenuAction::SaveClip => "Save clip now".to_string(),
            MenuAction::OpenConfig if view.config_exists => "Open config file".to_string(),
            MenuAction::OpenConfig => "Open the config folder".to_string(),
            MenuAction::Quit => "Quit localplay".to_string(),
        }
    }

    /// Whether this item can do anything in this state. A disabled item is the honest way to
    /// offer "save a clip" when there is no buffer to save one from.
    pub fn enabled(self, view: &TrayView<'_>) -> bool {
        match self {
            MenuAction::SaveClip => view.status.running,
            _ => true,
        }
    }

    /// Whether a separator goes above this item.
    pub fn separator_before(self) -> bool {
        matches!(self, MenuAction::Quit)
    }
}

// ---------------------------------------------------------------------------------------
// Dispatch — the tray menu, and what each item does
// ---------------------------------------------------------------------------------------

/// What an action needs from the shell.
///
/// Implemented for real in `lib.rs` (the window, the recorder, the platform file manager) and
/// by a recording fake in the tests below, so that "the user picked this item" → "these calls
/// happen, in this order" is pinned without a runtime and without a window.
pub trait Shell {
    fn window_visible(&self) -> Result<bool, String>;
    fn show_window(&self) -> Result<(), String>;
    fn hide_window(&self) -> Result<(), String>;
    fn status(&self) -> Result<RecordingStatusDto, CommandError>;
    fn start_recording(&self) -> Result<RecordingStatusDto, CommandError>;
    fn stop_recording(&self) -> Result<RecordingStatusDto, CommandError>;
    fn clip_now(&self) -> Result<RecordedClipDto, CommandError>;
    /// Reveal `config.toml` in the platform's file manager — or the folder it belongs in,
    /// when there is no file yet. Returns the path that was opened.
    fn open_config(&self) -> Result<PathBuf, String>;
    fn quit(&self) -> Result<(), String>;
}

/// What an action did. Every arm exists so a test can assert the *decision* rather than a
/// log line, and so the tray handler has something to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    WindowShown,
    WindowHidden,
    RecordingStarted,
    RecordingStopped,
    ClipSaved(String),
    ConfigOpened(PathBuf),
    Quitting,
    /// The action could not do what its label says. Never swallowed: the reason is what the
    /// tray handler logs and what the window shows.
    Refused(String),
}

impl Outcome {
    pub fn describe(&self) -> String {
        match self {
            Outcome::WindowShown => "the window is shown".to_string(),
            Outcome::WindowHidden => "the window is hidden; recording continues".to_string(),
            Outcome::RecordingStarted => "recording started".to_string(),
            Outcome::RecordingStopped => "recording stopped".to_string(),
            Outcome::ClipSaved(path) => format!("saved {}", clip_file_name(path)),
            Outcome::ConfigOpened(path) => format!("opened {}", path.display()),
            Outcome::Quitting => "quitting".to_string(),
            Outcome::Refused(reason) => reason.clone(),
        }
    }
}

/// Run one tray action against the shell.
pub fn dispatch(action: MenuAction, shell: &impl Shell) -> Outcome {
    match action {
        MenuAction::ShowHideWindow => match shell.window_visible() {
            Ok(true) => match shell.hide_window() {
                Ok(()) => Outcome::WindowHidden,
                Err(err) => Outcome::Refused(format!("could not hide the window: {err}")),
            },
            Ok(false) => match shell.show_window() {
                Ok(()) => Outcome::WindowShown,
                Err(err) => Outcome::Refused(format!("could not show the window: {err}")),
            },
            Err(err) => Outcome::Refused(format!("could not tell whether the window is up: {err}")),
        },

        MenuAction::ToggleRecording => match shell.status() {
            Ok(status) if status.running => match shell.stop_recording() {
                Ok(_) => Outcome::RecordingStopped,
                Err(err) => Outcome::Refused(err.message),
            },
            Ok(_) => match shell.start_recording() {
                Ok(_) => Outcome::RecordingStarted,
                Err(err) => Outcome::Refused(err.message),
            },
            Err(err) => Outcome::Refused(err.message),
        },

        MenuAction::SaveClip => match shell.clip_now() {
            Ok(clip) => Outcome::ClipSaved(clip.path),
            Err(err) => Outcome::Refused(err.message),
        },

        MenuAction::OpenConfig => match shell.open_config() {
            Ok(path) => Outcome::ConfigOpened(path),
            Err(err) => Outcome::Refused(err),
        },

        MenuAction::Quit => {
            // Stop first, through the same `RecorderHost::stop` the window's Stop button
            // calls: that is what flushes the encoder. A failure here is logged and does not
            // veto the quit the user asked for — but it is never silent.
            if let Err(err) = shell.stop_recording() {
                tracing::warn!(
                    "stopping the recorder before quitting failed: {} — quitting anyway",
                    err.message
                );
            }
            match shell.quit() {
                Ok(()) => Outcome::Quitting,
                Err(err) => Outcome::Refused(format!("could not quit: {err}")),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Closing the window
// ---------------------------------------------------------------------------------------

/// What a window close request does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// Hide the window and keep the process — and the recording — alive.
    Hide,
    /// End the process, flushing the encoder first.
    Quit,
}

/// **Hide. Always.**
///
/// A game clipper that exits when its window closes is a clipper that ends the recording
/// every time the user tidies their screen, so the X is a hide and quitting is an explicit
/// act (the tray's Quit, or the OS ending the process). This function exists as a function
/// so that the rule is executable — a later refactor has to delete a test to change it,
/// rather than adding an arm to a `match` in the wiring.
pub fn close_action() -> CloseAction {
    CloseAction::Hide
}

/// What the window tells its user *before* the first close, so that "the app disappeared" is
/// not a mystery. Shown in the recorder panel.
pub const CLOSE_HINT: &str = "Closing this window hides localplay in the tray; recording keeps \
                              running. Quit from the tray menu when you are done.";

// ---------------------------------------------------------------------------------------
// Autostart ([app] start_with_system)
// ---------------------------------------------------------------------------------------
//
// Windows only, by design: the shipping target is Windows and no other platform has a
// mechanism this project has any reason to own. The *decision* below is a plain function
// with an injectable process boundary, so it is tested; the effect needs a Windows host (see
// `docs/verification-status.md`).

/// The name of the value this application owns under the user's Run key. Nothing else in
/// that key is ever touched, so a manual entry a user added by hand survives.
pub const AUTOSTART_VALUE: &str = "localplay";

/// The per-user Run key. Per-user on purpose: no elevation, no machine-wide change, and it is
/// the first place a Windows user looks to see what starts with their session.
pub const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// One step of the sync, as a `reg.exe` invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegCommand {
    /// Read the value, to decide whether anything needs doing.
    Query,
    /// Write it (`/f` overwrites, so a moved install is repaired rather than duplicated).
    Enable,
    /// Remove it.
    Disable,
}

/// The arguments for one `reg.exe` invocation.
///
/// Pure, so the shape is pinned by a test on any host: the key path, the value name and the
/// quoting of an executable path that may contain spaces. What cannot be tested here is that
/// Windows acts on it.
pub fn reg_argv(cmd: RegCommand, value: &str, exe: &Path) -> Vec<String> {
    match cmd {
        RegCommand::Query => vec![
            "query".to_string(),
            RUN_KEY.to_string(),
            "/v".to_string(),
            value.to_string(),
        ],
        RegCommand::Enable => vec![
            "add".to_string(),
            RUN_KEY.to_string(),
            "/v".to_string(),
            value.to_string(),
            "/t".to_string(),
            "REG_SZ".to_string(),
            "/d".to_string(),
            format!("\"{}\"", exe.display()),
            "/f".to_string(),
        ],
        RegCommand::Disable => vec![
            "delete".to_string(),
            RUN_KEY.to_string(),
            "/v".to_string(),
            value.to_string(),
            "/f".to_string(),
        ],
    }
}

/// What a sync concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutostartOutcome {
    /// The entry was written.
    Enabled,
    /// A stale entry was removed.
    Removed,
    /// The entry already said what the config says; nothing was written.
    Unchanged,
    /// This platform has no autostart mechanism this project owns.
    NotApplicable(String),
}

impl AutostartOutcome {
    /// The line the startup log carries.
    pub fn describe(&self) -> String {
        match self {
            AutostartOutcome::Enabled => "the start-with-Windows entry is set".to_string(),
            AutostartOutcome::Removed => {
                "the start-with-Windows entry was removed (start_with_system = false)".to_string()
            }
            AutostartOutcome::Unchanged => {
                "the start-with-Windows entry already matches [app] start_with_system".to_string()
            }
            AutostartOutcome::NotApplicable(why) => why.clone(),
        }
    }
}

/// The executable path a `reg query` reported for `value`, if the value is present.
///
/// `reg query` prints one line per value:
/// `    localplay    REG_SZ    C:\path with spaces\localplay.exe`
/// and exits non-zero with "The system was unable to find the specified registry key or
/// value" when it is absent — in which case there is nothing to parse.
fn registered_path(output: &str, value: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next() == Some(value)).then(|| {
            // Drop the type column (`REG_SZ` / `REG_EXPAND_SZ`); the rest is the data.
            fields.skip(1).collect::<Vec<_>>().join(" ")
        })
    })
}

/// Bring the Windows autostart entry in line with `[app] start_with_system`.
///
/// The config file is the only switch — this project has no settings UI — so this is a
/// *sync*, not a one-way enable: `false` removes an entry this app wrote, and `true` writes
/// one. `run` is the process boundary (real `reg.exe` in [`apply_autostart`], a fake in the
/// tests), and it returns the command's stdout or a message describing how it failed.
pub fn sync_autostart(
    enabled: bool,
    exe: &Path,
    run: &mut impl FnMut(&[String]) -> Result<String, String>,
) -> Result<AutostartOutcome, String> {
    let listed = run(&reg_argv(RegCommand::Query, AUTOSTART_VALUE, exe))
        .map_err(|err| format!("could not read the Windows autostart entry: {err}"))?;
    let registered = registered_path(&listed, AUTOSTART_VALUE);
    let wanted = format!("\"{}\"", exe.display());

    match (enabled, registered) {
        (true, Some(path)) if path == wanted => Ok(AutostartOutcome::Unchanged),
        (true, _) => {
            run(&reg_argv(RegCommand::Enable, AUTOSTART_VALUE, exe))
                .map_err(|err| format!("could not write the Windows autostart entry: {err}"))?;
            Ok(AutostartOutcome::Enabled)
        }
        (false, Some(_)) => {
            run(&reg_argv(RegCommand::Disable, AUTOSTART_VALUE, exe))
                .map_err(|err| format!("could not remove the Windows autostart entry: {err}"))?;
            Ok(AutostartOutcome::Removed)
        }
        (false, None) => Ok(AutostartOutcome::Unchanged),
    }
}

/// Apply `[app] start_with_system` through `reg.exe`.
///
/// A failure is returned, never fatal: the caller logs it and the app keeps running — an
/// autostart entry that could not be written must not stop a recording session.
#[cfg(windows)]
pub fn apply_autostart(enabled: bool, exe: &Path) -> Result<AutostartOutcome, String> {
    let mut run = |argv: &[String]| -> Result<String, String> {
        let output = localplay_media::sidecar_command("reg")
            .args(argv)
            .output()
            .map_err(|err| format!("reg.exe could not be run: {err}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.status.success() {
            Ok(stdout)
        } else {
            // A `query` for an absent value is the normal "it is not set" answer, and the
            // caller reads the empty stdout as exactly that; anything else is reported.
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if argv.first().map(String::as_str) == Some("query") {
                Ok(String::new())
            } else {
                Err(format!("reg.exe exited with {}: {stderr}", output.status))
            }
        }
    };
    sync_autostart(enabled, exe, &mut run)
}

/// Off Windows there is no entry to keep in sync, and the shell says so rather than
/// pretending it did something.
#[cfg(not(windows))]
pub fn apply_autostart(_enabled: bool, _exe: &Path) -> Result<AutostartOutcome, String> {
    Ok(AutostartOutcome::NotApplicable(format!(
        "[app] start_with_system is a Windows feature; this build targets {} and has nothing \
         to register",
        std::env::consts::OS
    )))
}

// ---------------------------------------------------------------------------------------
// What the window shows about the background half
// ---------------------------------------------------------------------------------------

/// The shell's background half as the window asks for it once at startup.
///
/// Two things a user cannot otherwise discover live here: *which chord to press and whether
/// it is actually installed*, and *where the configuration it came from is on disk*. The
/// `close_hint` is a field rather than a sentence hard-coded in the frontend so that the
/// text explaining the X is the same text this module's tests pin ([`CLOSE_HINT`]).
///
/// Mirrored in `src/lib/types.ts`; `the_app_status_json_matches_the_typescript_interface`
/// pins the key set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppStatus {
    pub hotkey: HotkeyStatus,
    /// The `config.toml` this process read (`<app data>/config.toml`), so the window can
    /// name the file a user has to edit rather than describing a dialog that does not exist.
    pub config_path: String,
    /// False when that file does not exist and the example's values are in force.
    pub config_exists: bool,
    /// What closing the window does, in the words the panel shows.
    pub close_hint: String,
}

/// The tray as it should look: the whole visible surface as **one comparable value**.
///
/// The wiring compares two of these and only touches the OS when something really changed.
/// That matters because it runs twice a second, and on Windows every `set_tooltip` and
/// `set_icon` is a shell call — and because `==` on this struct is a claim a test can make:
/// the same state must render identically, or the tray would rewrite itself forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayRendering {
    pub state: TrayState,
    pub tooltip: String,
    pub window_label: String,
    pub recording_label: String,
    pub clip_label: String,
    pub clip_enabled: bool,
    pub config_label: String,
}

impl TrayView<'_> {
    /// The menu and the icon as this state renders them. Every field comes from the same
    /// [`MenuAction`] and [`TrayView`] functions the real menu is built from — there is no
    /// second spelling of a label anywhere.
    pub fn rendering(&self) -> TrayRendering {
        TrayRendering {
            state: self.tray_state(),
            tooltip: self.tooltip(),
            window_label: MenuAction::ShowHideWindow.label(self),
            recording_label: MenuAction::ToggleRecording.label(self),
            clip_label: MenuAction::SaveClip.label(self),
            clip_enabled: MenuAction::SaveClip.enabled(self),
            config_label: MenuAction::OpenConfig.label(self),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Revealing the config file
// ---------------------------------------------------------------------------------------

/// The file managers this shell knows how to hand a path to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileManager {
    /// `explorer`, with `/select,<path>` when the file exists.
    Explorer,
    /// `open`, with `-R <path>` when the file exists.
    MacOs,
    /// `xdg-open`, on the containing directory.
    XdgOpen,
}

impl FileManager {
    /// This host's file manager.
    pub fn current() -> Self {
        if cfg!(windows) {
            FileManager::Explorer
        } else if cfg!(target_os = "macos") {
            FileManager::MacOs
        } else {
            FileManager::XdgOpen
        }
    }
}

/// The program and arguments that reveal `path`: the file itself, selected, when it exists —
/// and its folder when it does not, because a user who has not created `config.toml` yet
/// needs somewhere to create it.
///
/// Pure, so a test pins the shape for **every** platform on any host, including the one this
/// project cannot run. What that test cannot do is watch a window open; the alternative
/// (`tauri-plugin-opener`) would add a dependency, a JavaScript package and a capability for
/// the same two lines.
pub fn reveal_command(path: &Path, manager: FileManager) -> (String, Vec<String>) {
    let folder = || {
        path.parent()
            .unwrap_or(path)
            .display()
            .to_string()
    };
    match (manager, path.is_file()) {
        (FileManager::Explorer, true) => {
            ("explorer".to_string(), vec![format!("/select,\"{}\"", path.display())])
        }
        (FileManager::Explorer, false) => ("explorer".to_string(), vec![folder()]),
        (FileManager::MacOs, true) => {
            ("open".to_string(), vec!["-R".to_string(), path.display().to_string()])
        }
        (FileManager::MacOs, false) => ("open".to_string(), vec![folder()]),
        // The generic case has nothing to select with: the folder is what a user can act on.
        (FileManager::XdgOpen, _) => ("xdg-open".to_string(), vec![folder()]),
    }
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn status(running: bool, error: Option<&str>) -> RecordingStatusDto {
        let mut status = RecordingStatusDto::from(localplay_recorder::RecorderStatus::stopped());
        status.running = running;
        status.error = error.map(str::to_string);
        status
    }

    fn clip(path: &str) -> RecordedClipDto {
        RecordedClipDto {
            id: Some(7),
            path: path.to_string(),
            duration_ms: 12_000,
            size_bytes: 1_024,
            codec: "h264_nvenc".to_string(),
            started_at_ms: 4_000,
        }
    }

    fn view<'a>(
        status: &'a RecordingStatusDto,
        hotkey: &'a HotkeyStatus,
        window_visible: bool,
    ) -> TrayView<'a> {
        TrayView { status, hotkey, window_visible, config_exists: true, last_press: None }
    }

    /// A `Shell` that records what it was asked to do, and can be told to fail.
    ///
    /// The visibility it reports is fixed at construction: each test builds the state its
    /// action is about, rather than driving a fake window through a sequence.
    struct FakeShell {
        calls: RefCell<Vec<&'static str>>,
        visible: bool,
        running: bool,
        /// The reason `clip_now` fails with, when it should.
        clip_fails: Option<&'static str>,
        /// The same for `start_recording` — the ffmpeg-missing case.
        start_fails: Option<&'static str>,
        /// The same for `stop_recording` — the flush-before-quit case.
        stop_fails: Option<&'static str>,
        quit_fails: bool,
        config_path: PathBuf,
    }

    impl Default for FakeShell {
        fn default() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                visible: false,
                running: false,
                clip_fails: None,
                start_fails: None,
                stop_fails: None,
                quit_fails: false,
                config_path: PathBuf::from("config.toml"),
            }
        }
    }

    impl FakeShell {
        fn recording() -> Self {
            Self { running: true, ..Self::default() }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.borrow().clone()
        }

        fn record(&self, call: &'static str) {
            self.calls.borrow_mut().push(call);
        }
    }

    impl Shell for FakeShell {
        fn window_visible(&self) -> Result<bool, String> {
            self.record("window_visible");
            Ok(self.visible)
        }
        fn show_window(&self) -> Result<(), String> {
            self.record("show_window");
            Ok(())
        }
        fn hide_window(&self) -> Result<(), String> {
            self.record("hide_window");
            Ok(())
        }
        fn status(&self) -> Result<RecordingStatusDto, CommandError> {
            self.record("status");
            Ok(status(self.running, None))
        }
        fn start_recording(&self) -> Result<RecordingStatusDto, CommandError> {
            self.record("start_recording");
            match self.start_fails {
                Some(message) => Err(CommandError::new(
                    crate::commands::ErrorCode::Recording,
                    message,
                )),
                None => Ok(status(true, None)),
            }
        }
        fn stop_recording(&self) -> Result<RecordingStatusDto, CommandError> {
            self.record("stop_recording");
            match self.stop_fails {
                Some(message) => Err(CommandError::new(
                    crate::commands::ErrorCode::Recording,
                    message,
                )),
                None => Ok(status(false, None)),
            }
        }
        fn clip_now(&self) -> Result<RecordedClipDto, CommandError> {
            self.record("clip_now");
            match self.clip_fails {
                Some(message) => Err(CommandError::new(
                    crate::commands::ErrorCode::Recording,
                    message,
                )),
                None => Ok(clip("C:\\clips\\clip-1.mp4")),
            }
        }
        fn open_config(&self) -> Result<PathBuf, String> {
            self.record("open_config");
            Ok(self.config_path.clone())
        }
        fn quit(&self) -> Result<(), String> {
            self.record("quit");
            if self.quit_fails {
                Err("the process refused to exit".to_string())
            } else {
                Ok(())
            }
        }
    }

    // -- the hotkey status ---------------------------------------------------------------

    #[test]
    fn a_chord_that_does_not_parse_is_reported_with_the_text_from_the_config() {
        let (hotkey, presses) = install_hotkey("Ctrl+Shift");

        assert!(!hotkey.installed);
        assert!(presses.is_none(), "nothing is listening when the chord does not parse");
        assert_eq!(hotkey.chord, "Ctrl+Shift", "the chord the user wrote is kept for display");
        let error = hotkey.error.expect("a failure carries its reason");
        assert!(error.contains("Ctrl+Shift"), "the reason names the chord: {error}");
        assert!(error.contains("Ctrl+F8"), "and shows the shape it wants: {error}");
    }

    #[test]
    fn the_hotkey_status_is_honest_about_what_this_build_can_do() {
        // On the development host there is no global hotkey to install, and the status says
        // so — with the chord it *would* have used, and with something to do instead. This
        // is also the only shape this test suite can produce: registering a real chord needs
        // Windows, and the suite never takes one (see `docs/verification-status.md`).
        let (hotkey, presses) = install_hotkey("ctrl+f8");

        assert_eq!(hotkey.chord, "Ctrl+F8", "the chord is normalised, as the CLI prints it");
        if hotkey::supported() {
            assert!(hotkey.installed, "on Windows the example chord registers");
            assert!(presses.is_some());
            assert_eq!(hotkey.error, None);
        } else {
            assert!(!hotkey.installed);
            assert!(presses.is_none());
            let error = hotkey.error.expect("not installing a hotkey has a reason");
            assert!(error.contains("Windows"), "the reason says why: {error}");
            assert!(error.contains("tray menu"), "and what to use instead: {error}");
        }
    }

    #[test]
    fn the_hotkey_status_json_matches_the_typescript_interface() {
        // `src/lib/types.ts` mirrors this struct by hand, like every other DTO here. Without
        // this test a rename on either side is discovered by a user staring at a panel that
        // says nothing about the key they are pressing.
        let value = serde_json::to_value(HotkeyStatus::installed("Ctrl+F8")).unwrap();
        let mut keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, ["chord", "error", "installed"]);
        assert_eq!(value["installed"], true);
        assert_eq!(value["error"], serde_json::Value::Null, "None is null, not a missing key");
    }

    // -- what the window shows about the background half ---------------------------------

    #[test]
    fn the_app_status_json_matches_the_typescript_interface() {
        let value = serde_json::to_value(AppStatus {
            hotkey: HotkeyStatus::installed("Ctrl+F8"),
            config_path: "C:\\Users\\player\\AppData\\Local\\localplay\\config.toml".to_string(),
            config_exists: false,
            close_hint: CLOSE_HINT.to_string(),
        })
        .unwrap();

        let mut keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, ["close_hint", "config_exists", "config_path", "hotkey"]);
        assert_eq!(value["hotkey"]["chord"], "Ctrl+F8", "the nested status crosses the wire too");
        assert_eq!(value["hotkey"]["installed"], true);
        assert_eq!(value["config_exists"], false);
    }

    #[test]
    fn the_same_state_renders_the_same_tray() {
        // The property the half-second poll depends on: equality of the rendering is how the
        // wiring decides that nothing needs writing. Two renderings of one state must be
        // equal, or the tray would rewrite its tooltip for ever.
        let hotkey = HotkeyStatus::installed("Ctrl+F8");
        let idle = status(false, None);

        assert_eq!(view(&idle, &hotkey, true).rendering(), view(&idle, &hotkey, true).rendering());
    }

    #[test]
    fn the_tray_rendering_moves_when_and_only_when_something_visible_did() {
        let hotkey = HotkeyStatus::installed("Ctrl+F8");
        let missing = HotkeyStatus::not_installed("Ctrl+F8", "another application owns it");
        let idle = status(false, None);
        let recording = status(true, None);

        let base = view(&idle, &hotkey, true).rendering();

        let hidden = view(&idle, &hotkey, false).rendering();
        assert_ne!(base, hidden, "hiding the window moves the first item");
        assert_eq!(base.window_label, "Hide the window");
        assert_eq!(hidden.window_label, "Show the window");
        assert_eq!(base.state, hidden.state, "but it is not a change of icon");

        let running = view(&recording, &hotkey, true).rendering();
        assert_ne!(base, running);
        assert_eq!(running.state, TrayState::Recording);
        assert_eq!(running.recording_label, "Stop recording");
        assert!(running.clip_enabled, "a running recorder can take a clip");
        assert!(!base.clip_enabled, "and one that is not running cannot");

        let broken_hotkey = view(&idle, &missing, true).rendering();
        assert_ne!(base, broken_hotkey);
        assert_eq!(broken_hotkey.state, TrayState::Attention);
        assert!(broken_hotkey.tooltip.contains("NOT installed"), "{}", broken_hotkey.tooltip);

        let mut pressed = view(&idle, &hotkey, true);
        pressed.last_press = Some("saved clip-1.mp4");
        assert_ne!(pressed.rendering(), base, "a press result moves the tooltip");
    }

    #[test]
    fn the_rendering_and_the_menu_are_the_same_labels() {
        // The wiring builds the real menu from `MenuAction::label` and decides what to
        // update from the rendering. If those ever disagreed, the tray would show one thing
        // and the code would believe another.
        let hotkey = HotkeyStatus::installed("Ctrl+F8");

        for (status, hotkey) in
            [(&status(true, None), &hotkey), (&status(false, None), &missing_hotkey())]
        {
            for visible in [true, false] {
                let v = view(status, hotkey, visible);
                let r = v.rendering();
                assert_eq!(r.state, v.tray_state());
                assert_eq!(r.tooltip, v.tooltip());
                assert_eq!(r.window_label, MenuAction::ShowHideWindow.label(&v));
                assert_eq!(r.recording_label, MenuAction::ToggleRecording.label(&v));
                assert_eq!(r.clip_label, MenuAction::SaveClip.label(&v));
                assert_eq!(r.clip_enabled, MenuAction::SaveClip.enabled(&v));
                assert_eq!(r.config_label, MenuAction::OpenConfig.label(&v));
            }
        }
    }

    fn missing_hotkey() -> HotkeyStatus {
        HotkeyStatus::not_installed("Ctrl+F8", "another application owns it")
    }

    // -- revealing the config file --------------------------------------------------------

    #[test]
    fn revealing_the_config_selects_it_or_opens_the_folder_that_should_hold_it() {
        // Both halves of the second sentence: a user editing a file that exists wants it
        // selected, and a user who has not created one yet needs the folder to create it in.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(&file, "[app]\n").unwrap();
        let missing = dir.path().join("not-created-yet.toml");
        let folder = dir.path().display().to_string();

        let (program, args) = reveal_command(&file, FileManager::Explorer);
        assert_eq!(program, "explorer");
        assert_eq!(args, vec![format!("/select,\"{}\"", file.display())]);
        assert_eq!(reveal_command(&missing, FileManager::Explorer).1, vec![folder.clone()]);

        let (program, args) = reveal_command(&file, FileManager::MacOs);
        assert_eq!(program, "open");
        assert_eq!(args, vec!["-R".to_string(), file.display().to_string()]);
        assert_eq!(reveal_command(&missing, FileManager::MacOs).1, vec![folder.clone()]);

        let (program, args) = reveal_command(&file, FileManager::XdgOpen);
        assert_eq!(program, "xdg-open");
        assert_eq!(args, vec![folder], "the generic case has nothing to select with");
    }

    #[test]
    fn the_file_manager_is_this_hosts_own() {
        // The one part that is not data: which of the three this build would actually run.
        let dir = tempfile::tempdir().unwrap();
        let (program, _) = reveal_command(&dir.path().join("config.toml"), FileManager::current());

        let expected =
            if cfg!(windows) { "explorer" } else if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        assert_eq!(program, expected);
    }

    // -- one press, one clip -------------------------------------------------------------

    #[test]
    fn a_press_takes_exactly_one_clip() {
        // The property the whole feature rests on: one keypress is one `clip_now`, never two.
        // (The engine's own trigger is what actually splices; this pins the call count.)
        let calls = RefCell::new(0);
        let outcome = on_hotkey_press(|| {
            *calls.borrow_mut() += 1;
            Ok(clip("C:\\clips\\clip-1.mp4"))
        });

        assert_eq!(*calls.borrow(), 1, "exactly one clip per press");
        assert_eq!(outcome, PressOutcome::Clip(clip("C:\\clips\\clip-1.mp4")));
        assert_eq!(outcome.describe(), "saved clip-1.mp4");
        assert!(outcome.saved_a_clip());
    }

    #[test]
    fn a_press_that_fails_reports_the_engines_reason_and_does_not_retry() {
        // "Nothing is recording" is a press that did nothing; the user gets the engine's own
        // sentence (the tray tooltip carries it) rather than a second attempt at a clip.
        let calls = RefCell::new(0);
        let outcome = on_hotkey_press(|| {
            *calls.borrow_mut() += 1;
            Err(CommandError::new(
                crate::commands::ErrorCode::Recording,
                "nothing is recording, so there is no buffer to take a clip from",
            ))
        });

        assert_eq!(*calls.borrow(), 1, "a failed press is not retried");
        assert!(!outcome.saved_a_clip());
        assert!(outcome.describe().contains("nothing is recording"), "{}", outcome.describe());
    }

    // -- the tray menu -------------------------------------------------------------------

    #[test]
    fn every_menu_action_has_a_unique_stable_id_and_round_trips() {
        // These strings are what the tray event carries; a duplicate or a rename would make
        // one menu item dispatch another's action.
        let ids: Vec<&str> = MenuAction::ALL.iter().map(|a| a.id()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "ids are unique: {ids:?}");

        for action in MenuAction::ALL {
            assert_eq!(MenuAction::from_id(action.id()), Some(action));
        }
        assert_eq!(MenuAction::from_id("something-else"), None);
        assert_eq!(MenuAction::ALL.len(), 5, "the five items the tray offers");
    }

    #[test]
    fn the_labels_follow_the_state_they_describe() {
        let idle = status(false, None);
        let running = status(true, None);
        let hotkey = HotkeyStatus::installed("Ctrl+F8");

        let shown = view(&idle, &hotkey, true);
        let hidden = view(&idle, &hotkey, false);
        assert_eq!(MenuAction::ShowHideWindow.label(&shown), "Hide the window");
        assert_eq!(MenuAction::ShowHideWindow.label(&hidden), "Show the window");

        let recording = view(&running, &hotkey, true);
        assert_eq!(MenuAction::ToggleRecording.label(&shown), "Start recording");
        assert_eq!(MenuAction::ToggleRecording.label(&recording), "Stop recording");

        assert_eq!(MenuAction::SaveClip.label(&recording), "Save clip now");
        assert_eq!(MenuAction::Quit.label(&shown), "Quit localplay");
        assert!(MenuAction::Quit.separator_before(), "quit is set apart from the rest");
        assert!(!MenuAction::SaveClip.separator_before());
    }

    #[test]
    fn saving_a_clip_is_only_offered_while_there_is_a_buffer() {
        let hotkey = HotkeyStatus::installed("Ctrl+F8");

        assert!(
            MenuAction::SaveClip.enabled(&view(&status(true, None), &hotkey, true)),
            "a running recorder can take a clip"
        );
        assert!(
            !MenuAction::SaveClip.enabled(&view(&status(false, None), &hotkey, true)),
            "with nothing recording there is no buffer to take one from"
        );
        assert!(
            MenuAction::ToggleRecording.enabled(&view(&status(false, None), &hotkey, true)),
            "and the chance to start one is always offered"
        );
    }

    #[test]
    fn the_config_item_says_which_thing_it_opens() {
        let hotkey = HotkeyStatus::installed("Ctrl+F8");
        let idle = status(false, None);

        let with_file = view(&idle, &hotkey, true);
        let mut without = view(&idle, &hotkey, true);
        without.config_exists = false;

        assert_eq!(MenuAction::OpenConfig.label(&with_file), "Open config file");
        assert_eq!(MenuAction::OpenConfig.label(&without), "Open the config folder");
    }

    // -- the tray state, icon and tooltip ------------------------------------------------

    #[test]
    fn the_tray_state_tracks_the_recorder_and_the_hotkey() {
        let hotkey = HotkeyStatus::installed("Ctrl+F8");
        let missing = HotkeyStatus::not_installed("Ctrl+F8", "another application owns it");

        assert_eq!(view(&status(false, None), &hotkey, true).tray_state(), TrayState::Idle);
        assert_eq!(
            view(&status(true, None), &hotkey, true).tray_state(),
            TrayState::Recording
        );
        assert_eq!(
            view(&status(false, Some("the encoder died")), &hotkey, true).tray_state(),
            TrayState::Attention,
            "a recorder that stopped for a failure is not 'idle'"
        );
        assert_eq!(
            view(&status(false, None), &missing, true).tray_state(),
            TrayState::Attention,
            "and neither is a hotkey that cannot fire"
        );
    }

    #[test]
    fn the_tooltip_names_the_chord_or_the_problem() {
        let hotkey = HotkeyStatus::installed("Ctrl+F8");
        let missing = HotkeyStatus::not_installed("Ctrl+F8", "another application owns it");

        let idle = status(false, None);
        let recording = status(true, None);
        let broken = status(false, Some("the encoder exited"));

        assert_eq!(
            view(&idle, &hotkey, true).tooltip(),
            "localplay — not recording. Press Ctrl+F8 for a clip once recording is on."
        );
        assert_eq!(
            view(&recording, &hotkey, true).tooltip(),
            "localplay — recording. Press Ctrl+F8 to take a clip."
        );
        let stopped = view(&broken, &hotkey, true).tooltip();
        assert!(stopped.contains("the encoder exited"), "{stopped}");

        let not_installed = view(&idle, &missing, true).tooltip();
        assert!(not_installed.contains("Ctrl+F8"), "the chord that failed: {not_installed}");
        assert!(not_installed.contains("NOT installed"), "{not_installed}");
    }

    #[test]
    fn the_tooltip_carries_the_last_press_and_stays_inside_the_os_limit() {
        let hotkey = HotkeyStatus::installed("Ctrl+F8");
        let recording = status(true, None);
        let mut with_press = view(&recording, &hotkey, true);
        with_press.last_press = Some("saved clip-2026-09-23_19-31-02_saved_from_the_window.mp4");
        let tooltip = with_press.tooltip();
        assert!(tooltip.contains("Last press: saved clip-2026-09-23_19-31-02"), "{tooltip}");
        assert!(tooltip.chars().count() <= TOOLTIP_LIMIT, "{tooltip}");

        // A pathological reason cannot push it past the limit either — and the cut says so.
        let long = status(false, Some(&"x".repeat(400)));
        let long_press = "y".repeat(400);
        let mut noisy = view(&long, &hotkey, true);
        noisy.last_press = Some(&long_press);
        let tooltip = noisy.tooltip();
        assert_eq!(tooltip.chars().count(), TOOLTIP_LIMIT, "{tooltip}");
        assert!(tooltip.ends_with('…'), "a truncated tooltip is marked: {tooltip}");
    }

    // -- dispatch: the menu action → the shell calls -------------------------------------

    #[test]
    fn showing_and_hiding_follows_what_is_on_screen() {
        let shell = FakeShell { visible: true, ..FakeShell::default() };
        assert_eq!(dispatch(MenuAction::ShowHideWindow, &shell), Outcome::WindowHidden);
        assert_eq!(shell.calls(), ["window_visible", "hide_window"]);

        let shell = FakeShell { visible: false, ..FakeShell::default() };
        assert_eq!(dispatch(MenuAction::ShowHideWindow, &shell), Outcome::WindowShown);
        assert_eq!(shell.calls(), ["window_visible", "show_window"]);
    }

    #[test]
    fn the_recording_item_starts_or_stops_according_to_the_engine() {
        let idle = FakeShell::default();
        assert_eq!(dispatch(MenuAction::ToggleRecording, &idle), Outcome::RecordingStarted);
        assert_eq!(idle.calls(), ["status", "start_recording"]);

        let running = FakeShell::recording();
        assert_eq!(dispatch(MenuAction::ToggleRecording, &running), Outcome::RecordingStopped);
        assert_eq!(running.calls(), ["status", "stop_recording"]);
    }

    #[test]
    fn a_start_the_engine_refuses_is_a_reported_refusal_not_a_silent_no() {
        // The typical case: ffmpeg is missing, or the encoder cannot be opened.
        let shell = FakeShell {
            start_fails: Some("ffmpeg was not found, so a recording cannot be started"),
            ..FakeShell::default()
        };

        let outcome = dispatch(MenuAction::ToggleRecording, &shell);
        assert_eq!(
            outcome,
            Outcome::Refused("ffmpeg was not found, so a recording cannot be started".to_string())
        );
        assert_eq!(outcome.describe(), "ffmpeg was not found, so a recording cannot be started");
        assert_eq!(shell.calls(), ["status", "start_recording"], "and it does not try again");
    }

    #[test]
    fn the_save_item_takes_a_clip_and_reports_it() {
        let shell = FakeShell::recording();
        let outcome = dispatch(MenuAction::SaveClip, &shell);

        assert_eq!(outcome, Outcome::ClipSaved("C:\\clips\\clip-1.mp4".to_string()));
        assert_eq!(outcome.describe(), "saved clip-1.mp4");
        assert_eq!(shell.calls(), ["clip_now"], "the tray's clip is the same call as the button's");
    }

    #[test]
    fn a_clip_that_cannot_be_taken_says_so() {
        let shell = FakeShell {
            clip_fails: Some("nothing is recording, so there is no buffer to take a clip from"),
            ..FakeShell::default()
        };

        let outcome = dispatch(MenuAction::SaveClip, &shell);
        assert!(
            outcome.describe().contains("nothing is recording"),
            "the engine's reason reaches the user: {}",
            outcome.describe()
        );
    }

    #[test]
    fn the_config_item_opens_the_file_and_reports_the_path() {
        let shell = FakeShell {
            config_path: PathBuf::from("C:\\Users\\player\\AppData\\Local\\localplay\\config.toml"),
            ..FakeShell::default()
        };

        let outcome = dispatch(MenuAction::OpenConfig, &shell);
        assert_eq!(outcome, Outcome::ConfigOpened(shell.config_path.clone()));
        assert!(outcome.describe().contains("config.toml"), "{}", outcome.describe());
        assert_eq!(shell.calls(), ["open_config"]);
    }

    #[test]
    fn quitting_flushes_the_encoder_first() {
        // A quit while recording must stop the recorder — that is the call that flushes
        // ffmpeg and closes the capture session — *before* the process goes away.
        let shell = FakeShell::recording();
        let outcome = dispatch(MenuAction::Quit, &shell);

        assert_eq!(outcome, Outcome::Quitting);
        assert_eq!(shell.calls(), ["stop_recording", "quit"], "stop first, then exit");
    }

    #[test]
    fn a_quit_still_quits_when_the_stop_fails() {
        // The stop is best-effort: the user asked to quit, and a failure to flush must not
        // leave a process running that they cannot see. It is logged, not swallowed.
        let shell =
            FakeShell { stop_fails: Some("the encoder refused to flush"), ..FakeShell::recording() };

        assert_eq!(dispatch(MenuAction::Quit, &shell), Outcome::Quitting);
        assert_eq!(shell.calls(), ["stop_recording", "quit"], "it flushed first, then left");
    }

    #[test]
    fn a_quit_that_the_process_refuses_is_reported() {
        let shell = FakeShell { quit_fails: true, ..FakeShell::recording() };
        let outcome = dispatch(MenuAction::Quit, &shell);

        assert!(
            outcome.describe().contains("could not quit"),
            "a refused quit is not reported as a quit: {}",
            outcome.describe()
        );
    }

    #[test]
    fn every_action_reaches_a_handler_and_none_is_left_unmapped() {
        // The exhaustiveness is the compiler's (`dispatch` matches every arm), and this pins
        // that each one produces something the caller can log rather than an empty string.
        let idle = FakeShell::default();
        let running = FakeShell::recording();

        for action in MenuAction::ALL {
            let shell = if matches!(action, MenuAction::ShowHideWindow) { &idle } else { &running };
            let outcome = dispatch(action, shell);
            assert!(!outcome.describe().is_empty(), "{action:?} must describe itself");
            assert!(!shell.calls().is_empty(), "{action:?} must have called the shell");
        }
    }

    // -- closing the window --------------------------------------------------------------

    #[test]
    fn closing_the_window_hides_it_and_never_quits() {
        // The rule this whole file exists for: a clipper that exits when its window closes
        // ends the recording every time the user tidies their screen.
        assert_eq!(close_action(), CloseAction::Hide);
        assert_eq!(close_action(), CloseAction::Hide, "and it is not conditional");
    }

    #[test]
    fn the_close_hint_says_where_the_window_went_and_how_to_leave() {
        assert!(CLOSE_HINT.contains("tray"), "{CLOSE_HINT}");
        assert!(CLOSE_HINT.contains("recording keeps running"), "{CLOSE_HINT}");
        assert!(CLOSE_HINT.contains("Quit from the tray menu"), "{CLOSE_HINT}");
    }

    // -- autostart -----------------------------------------------------------------------

    #[test]
    fn the_run_key_command_has_the_shape_windows_expects() {
        // `reg.exe` on the per-user Run key, with the executable quoted because an install
        // path may contain spaces. Idempotent by `/f`, so a moved install is repaired.
        let exe = Path::new(r"C:\Program Files\localplay\localplay.exe");

        assert_eq!(
            reg_argv(RegCommand::Query, AUTOSTART_VALUE, exe),
            vec!["query", RUN_KEY, "/v", "localplay"]
        );
        assert_eq!(
            reg_argv(RegCommand::Enable, AUTOSTART_VALUE, exe),
            vec![
                "add",
                RUN_KEY,
                "/v",
                "localplay",
                "/t",
                "REG_SZ",
                "/d",
                r#""C:\Program Files\localplay\localplay.exe""#,
                "/f",
            ]
        );
        assert_eq!(
            reg_argv(RegCommand::Disable, AUTOSTART_VALUE, exe),
            vec!["delete", RUN_KEY, "/v", "localplay", "/f"]
        );

        assert!(RUN_KEY.contains(r"HKCU\"), "per-user, so it needs no elevation: {RUN_KEY}");
        assert!(RUN_KEY.ends_with(r"\Run"), "{RUN_KEY}");
    }

    /// A `reg.exe` that answers from memory: the value it reports for `query`, and a log of
    /// everything it was asked to run.
    struct FakeReg {
        present: Option<String>,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl FakeReg {
        fn new(present: Option<&str>) -> Self {
            Self { present: present.map(str::to_string), calls: RefCell::new(Vec::new()) }
        }

        fn runner(&self) -> impl FnMut(&[String]) -> Result<String, String> + '_ {
            move |argv: &[String]| {
                self.calls.borrow_mut().push(argv.to_vec());
                match argv.first().map(String::as_str) {
                    Some("query") => Ok(match &self.present {
                        Some(path) => format!(
                            "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\r\n    localplay    REG_SZ    {path}\r\n\r\n"
                        ),
                        None => String::new(),
                    }),
                    Some(_) => Ok(String::new()),
                    None => Err("no arguments".to_string()),
                }
            }
        }

        fn commands(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(|argv| argv.first().cloned().unwrap_or_default())
                .collect()
        }
    }

    #[test]
    fn autostart_writes_the_entry_when_the_config_asks_for_it() {
        let exe = Path::new(r"C:\localplay\localplay.exe");
        let reg = FakeReg::new(None);
        let mut run = reg.runner();

        let outcome = sync_autostart(true, exe, &mut run).unwrap();

        assert_eq!(outcome, AutostartOutcome::Enabled);
        assert_eq!(reg.commands(), ["query", "add"], "it reads first, then writes");
    }

    #[test]
    fn autostart_leaves_a_correct_entry_alone() {
        // The common case on every start after the first: no registry write at all.
        let exe = Path::new(r"C:\localplay\localplay.exe");
        let reg = FakeReg::new(Some(r#""C:\localplay\localplay.exe""#));
        let mut run = reg.runner();

        let outcome = sync_autostart(true, exe, &mut run).unwrap();

        assert_eq!(outcome, AutostartOutcome::Unchanged);
        assert_eq!(reg.commands(), ["query"], "nothing is written when nothing changed");
    }

    #[test]
    fn autostart_repairs_an_entry_that_points_somewhere_else() {
        // The app moved, or an older install wrote it: `enabled = true` must end with the
        // entry naming *this* executable.
        let exe = Path::new(r"D:\games\localplay\localplay.exe");
        let reg = FakeReg::new(Some(r#""C:\old\localplay.exe""#));
        let mut run = reg.runner();

        assert_eq!(sync_autostart(true, exe, &mut run).unwrap(), AutostartOutcome::Enabled);
        assert_eq!(reg.commands(), ["query", "add"]);
    }

    #[test]
    fn autostart_removes_the_entry_when_the_config_says_false() {
        let exe = Path::new(r"C:\localplay\localplay.exe");
        let reg = FakeReg::new(Some(r#""C:\localplay\localplay.exe""#));
        let mut run = reg.runner();

        let outcome = sync_autostart(false, exe, &mut run).unwrap();

        assert_eq!(outcome, AutostartOutcome::Removed);
        assert_eq!(reg.commands(), ["query", "delete"]);
    }

    #[test]
    fn autostart_never_touches_the_key_when_there_is_nothing_to_remove() {
        // `false` on a machine that never had one: no delete, and therefore no chance of
        // removing an entry this app did not write.
        let exe = Path::new(r"C:\localplay\localplay.exe");
        let reg = FakeReg::new(None);
        let mut run = reg.runner();

        assert_eq!(sync_autostart(false, exe, &mut run).unwrap(), AutostartOutcome::Unchanged);
        assert_eq!(reg.commands(), ["query"]);
    }

    #[test]
    fn a_registry_failure_is_reported_rather_than_treated_as_absent() {
        // A `reg.exe` that cannot run at all must not be read as "there is no entry": that
        // would make a failed *read* look like a successful "nothing to do".
        let exe = Path::new(r"C:\localplay\localplay.exe");
        let mut run = |_: &[String]| Err("reg.exe could not be run: not found".to_string());

        let err = sync_autostart(true, exe, &mut run).unwrap_err();
        assert!(err.contains("could not be run"), "{err}");
    }

    #[cfg(not(windows))]
    #[test]
    fn off_windows_autostart_is_not_applicable_and_says_so() {
        // No file, no plist, no launch agent: nothing outside this process is written on a
        // host this feature does not target, and the caller gets a sentence to log.
        //
        // The Windows half of `apply_autostart` is deliberately NOT exercised by this suite
        // on any host: `enabled = true` on Windows writes the real Run key of whoever ran the
        // tests, which is not a test's business. It is compiled by the cross-check
        // (`cargo check --target x86_64-pc-windows-msvc`) and belongs on the manual list in
        // `docs/verification-status.md`.
        let outcome = apply_autostart(true, Path::new("/usr/local/bin/localplay")).unwrap();

        match outcome {
            AutostartOutcome::NotApplicable(why) => {
                assert!(why.contains("Windows feature"), "{why}");
                assert!(why.contains("nothing to register"), "{why}");
            }
            other => panic!("off Windows there is nothing to apply, got {other:?}"),
        }
    }
}
