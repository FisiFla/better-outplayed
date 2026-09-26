//! Where the desktop shell reads its storage settings from, and where it keeps its files.
//!
//! # Why this is not `localplay_cli::config::Config`
//!
//! The CLI owns the full configuration model (spec §10) and this module deliberately does
//! not duplicate it. Two reasons, both load-bearing:
//!
//! 1. The desktop shell reads only what the review window needs: `[storage]`, plus
//!    `[encode]`, `[mic]` and `[games]` for the settings panel and for the options a start
//!    is built with. It captures nothing, so `[buffer]`, `[audio]`, `[hotkeys]` and
//!    `[events]` are none of its business — and re-validating them here would let a
//!    capture-only mistake (say `vendor = "software"`) refuse to open the review UI, which
//!    has no encoder in it.
//! 2. Depending on `localplay-cli` to borrow its `Config` would pull the whole capture
//!    stack — `wgc`, `wasapi`, the ffmpeg encoder child — into the GUI binary for the sake
//!    of a handful of fields.
//!
//! # The one invariant that keeps the window usable
//!
//! **Reading applies the engine's rules; writing may add the panel's own.**
//! [`read_effective_settings`] has to accept every value a start accepts, because the
//! settings panel renders only once it succeeds: a reader stricter than the engine is a
//! panel that will not open, and so cannot repair the file that stopped it. The panel is
//! still allowed stricter refusals of its own on the way *in* (`validate_edit`: the fps
//! band, and the `clips_dir` hygiene that keeps the asset-protocol scope off whole
//! volumes), because those refuse a value the user already has on screen to look at.
//!
//! That asymmetry is not decoration; getting it wrong shipped. The panel's own copy of the
//! `output_size` rule added a frame bound the engine does not have and trimmed nothing, so
//! a file carrying `" 1920x1080"` recorded perfectly while leaving the settings panel
//! permanently blank, unrepairable and silent.
//!
//! What must NOT drift is the *location* the two binaries agree on, which is why
//! [`app_data_dir`] repeats the CLI's rule verbatim instead of inventing one: the CLI
//! writes `localplay.db` in that directory and the shell reads the same file. A test below
//! pins the parse of the repository's real `config.example.toml` against that section, so
//! a change to the example cannot silently diverge from what this module deserialises.
//!
//! A full adapter that reads the whole file through the CLI's own type would be the better
//! end state; it is a bigger change than this pass should make to a crate that already
//! works, and it is named here rather than done. It is also the honest way to close the gap
//! the invariant above describes: the engine's rules would then be *borrowed* rather than
//! mirrored by hand, and the two could not drift at all.

use crate::commands::{CommandError, ErrorCode};
use localplay_events::process::{default_watch, GamesSection};
use localplay_recorder::config::{BufferSection, EncodeSection, MicSection, StorageSection};
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

/// The `[mic]` and `[games]` sections — what a recording is started *with*.
///
/// Read at start time like the capture sections: a review pane needs neither a
/// microphone nor a game watcher. The types are the engine's own, so one file means
/// one thing to the window and the CLI. Both sections default when absent
/// (`MicSection` off, `GamesSection` with `auto_record = false` and the default watch
/// list); a section that is present but mis-typed is an error naming it, like
/// everywhere else in this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineOptionsConfig {
    pub mic: MicSection,
    pub games: GamesSection,
}

impl EngineOptionsConfig {
    pub fn example() -> Result<Self, CommandError> {
        Self::from_toml(EXAMPLE_CONFIG)
    }

    pub fn from_toml(text: &str) -> Result<Self, CommandError> {
        #[derive(Deserialize)]
        struct File {
            #[serde(default)]
            mic: MicSection,
            #[serde(default)]
            games: GamesSection,
        }
        toml::from_str::<File>(text)
            .map(|f| Self { mic: f.mic, games: f.games })
            .map_err(|err| {
                CommandError::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "the config file's [mic] or [games] section could not be read: \
                         {err}"
                    ),
                )
            })
    }

    pub fn load(path: &Path) -> Result<Self, CommandError> {
        if !path.is_file() {
            // The siblings all say so on this path — see `read_effective_settings` for why
            // this one stopped being quiet.
            tracing::info!(
                "no config file at {}: starting with the example's [mic] and [games]",
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

/// Bounds for `[encode] fps`, and **the panel's own policy rather than the engine's**:
/// nothing anywhere in the engine bounds `fps` (it is a bare `u32`, and the capture
/// rate is adapted to what the machine measures), so these apply on the *write* path
/// only — refusing a value while the user's hand is still on it.
///
/// They deliberately do **not** apply to [`read_effective_settings`]. A reader stricter
/// than a start is a settings panel that never renders, and with no panel there is no
/// way to repair the file from the UI — see that function's docs.
pub const FPS_MIN: u32 = 1;
pub const FPS_MAX: u32 = 240;

/// One edited key. Typed by construction: a caller cannot smuggle a string into a bool,
/// so the writer never parses user text into TOML values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingEdit {
    /// `[storage] clips_dir`. Empty resets to the application-data default.
    ClipsDir(String),
    /// `[encode] fps`.
    Fps(u32),
    /// `[encode] output_size`. Empty means the native capture size.
    OutputSize(String),
    /// `[mic] enabled`.
    MicEnabled(bool),
    /// `[games] auto_record`.
    AutoRecord(bool),
}

impl SettingEdit {
    pub(crate) fn key(&self) -> &'static str {
        match self {
            SettingEdit::ClipsDir(_) => "storage.clips_dir",
            SettingEdit::Fps(_) => "encode.fps",
            SettingEdit::OutputSize(_) => "encode.output_size",
            SettingEdit::MicEnabled(_) => "mic.enabled",
            SettingEdit::AutoRecord(_) => "games.auto_record",
        }
    }
}

/// The settings the window shows and edits, with per-key provenance.
///
/// Values come from the file when it sets them and from the compiled-in example
/// otherwise; `defaulted` names the keys that fell back (e.g. `"encode.fps"`), so the
/// panel can say so instead of presenting example values as configured ones.
///
/// A key that is present but mis-typed is an error naming it, and so is an
/// `output_size` the engine's parser refuses — both are contracts a start keeps too.
/// Anything else is reported as the file has it, including values the *panel* would
/// refuse to write (an out-of-band fps, a relative clips_dir): the panel has to render
/// to be able to repair them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveSettings {
    pub clips_dir: String,
    pub fps: u32,
    pub output_size: String,
    pub mic_enabled: bool,
    pub auto_record: bool,
    /// Display names from the file's `[[games.watch]]`, or the default watch list when
    /// the file names none (which is what the engine watches then, too).
    pub watch_titles: Vec<String>,
    pub defaulted: Vec<&'static str>,
    pub config_exists: bool,
}

/// The message for a key that is present but the wrong shape.
///
/// `what` names the shape the key *should* have been: a bool for the two switches, a
/// string for the two paths, an integer for the rate, and an array of tables for
/// `games.watch` — which is not a bool and used to say it was.
fn mistyped(key: &'static str) -> CommandError {
    CommandError::new(
        ErrorCode::InvalidInput,
        format!(
            "the config file's [{section}] {key} is not a {what}, so it cannot be used",
            section = key.split('.').next().unwrap_or("config"),
            what = match key {
                "storage.clips_dir" | "encode.output_size" => "string",
                "encode.fps" => "integer",
                "mic.enabled" | "games.auto_record" => "true/false value",
                "games.watch" => "list of [[games.watch]] tables",
                _ => "value of the shape this key takes",
            },
        ),
    )
}

fn table<'a>(doc: &'a toml_edit::DocumentMut, key: &str) -> Option<&'a toml_edit::Item> {
    doc.get(key)
}

fn get_str(
    doc: &toml_edit::DocumentMut,
    table_key: &str,
    value_key: &'static str,
    full_key: &'static str,
) -> Result<Option<String>, CommandError> {
    match table(doc, table_key).and_then(|t| t.get(value_key)) {
        None => Ok(None),
        Some(item) => {
            item.as_str().map(str::to_string).map(Some).ok_or_else(|| mistyped(full_key))
        }
    }
}

fn get_int(
    doc: &toml_edit::DocumentMut,
    table_key: &str,
    value_key: &'static str,
    full_key: &'static str,
) -> Result<Option<i64>, CommandError> {
    match table(doc, table_key).and_then(|t| t.get(value_key)) {
        None => Ok(None),
        Some(item) => item.as_integer().map(Some).ok_or_else(|| mistyped(full_key)),
    }
}

fn get_bool(
    doc: &toml_edit::DocumentMut,
    table_key: &str,
    value_key: &'static str,
    full_key: &'static str,
) -> Result<Option<bool>, CommandError> {
    match table(doc, table_key).and_then(|t| t.get(value_key)) {
        None => Ok(None),
        Some(item) => item.as_bool().map(Some).ok_or_else(|| mistyped(full_key)),
    }
}

/// An absolute path on either platform the application ships to: a POSIX root, an
/// extended/UNC prefix, or a drive letter. The development host is macOS, where
/// `Path::is_absolute` would reject a Windows path the real machine accepts, so this
/// checks both shapes explicitly rather than asking the host.
fn is_absolute_either(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some('/') | Some('\\') => true,
        Some(drive) if drive.is_ascii_alphabetic() => match (chars.next(), chars.next()) {
            (Some(':'), Some('/' | '\\')) => true,
            _ => false,
        },
        _ => false,
    }
}

/// The panel's own ergonomic band on `[encode] fps` — see `FPS_MIN`. Write-path only.
fn validate_fps(fps: u32) -> Result<(), CommandError> {
    if (FPS_MIN..=FPS_MAX).contains(&fps) {
        Ok(())
    } else {
        Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "encode.fps is {fps}, but the capture rate must be between {FPS_MIN} and \
                 {FPS_MAX}"
            ),
        ))
    }
}

/// `[encode] output_size`, decided by **the engine's own parser** so the panel refuses
/// exactly what a start refuses and accepts exactly what it accepts.
///
/// Reused rather than re-implemented, and that is the whole point. The copy that used to
/// live here trimmed nothing and added a `16..=16384` bound the engine does not have, so
/// `output_size = " 1920x1080"` recorded perfectly while `get_settings` failed on it —
/// and a failed `get_settings` is a settings panel that never renders.
fn validate_output_size(size: &str) -> Result<(), CommandError> {
    localplay_recorder::parse_output_size(size).map(|_| ()).map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("the config file's encode.output_size is not usable: {err:#}"),
        )
    })
}

/// Why a clips directory is refused, or `None` when it is acceptable.
///
/// `[storage] clips_dir` is user-supplied and is then handed to
/// `allow_directory(&dir, true)`, which widens the asset protocol's scope — the window's
/// only read channel, since `capabilities/default.json` grants it no filesystem
/// permission. A filesystem root, a share with nothing inside it, or a `..` segment
/// would widen that scope far past the directory the user named: `clips_dir = "C:\\"`
/// serves the whole drive to the webview.
///
/// So it must be absolute, and then something *inside* a volume:
///   * no `..` segment anywhere — the scope is resolved, not canonicalised, so a path
///     that climbs back out is not the directory it appears to be;
///   * not a root — `/`, `\`, `C:/`, and (behind its prefix) `\\?\C:/`;
///   * for a share, not the bare `\\server\share`: it must name a directory inside one.
///
/// What this is **not** is the engine's rule. `resolve_dir` takes the string as given and
/// validates nothing, so this — like the fps band — belongs to the write path alone.
/// Applying it on read would make the panel unrenderable for a file the recorder runs.
fn clips_dir_problem(dir: &str) -> Option<&'static str> {
    // One shape to reason about: both platforms separate with these, and which one the
    // string used says nothing about whether it names a whole volume.
    let slashed = dir.replace('\\', "/");
    // `\\?\C:\...` is the extended-length spelling of the same path; the root hiding
    // behind the prefix must not slip past the check below.
    let stripped = slashed.strip_prefix("//?/").unwrap_or(slashed.as_str());
    let segments: Vec<&str> = stripped.split('/').filter(|s| !s.is_empty()).collect();

    if segments.iter().any(|segment| *segment == "..") {
        return Some("names a parent-directory segment");
    }
    // A UNC path is `//server/share/...`; fewer than three segments names a host, or a
    // share, rather than a directory inside one.
    let is_share = stripped.starts_with("//");
    let is_root = if is_share {
        segments.len() < 3
    } else {
        // `/` leaves no segment at all; `C:/` leaves the drive designator alone.
        segments.is_empty() || (segments.len() == 1 && segments[0].ends_with(':'))
    };
    if is_root {
        return Some(if is_share {
            "a share with no directory inside it"
        } else {
            "a filesystem root"
        });
    }
    None
}

/// `[storage] clips_dir` as the panel will *write* it: empty (the default), or an
/// absolute path to a directory inside a volume. See `clips_dir_problem` for why the
/// roots, the bare shares and the parent-directory segments are refused.
fn validate_clips_dir(dir: &str) -> Result<(), CommandError> {
    if dir.is_empty() {
        return Ok(());
    }
    let problem = if is_absolute_either(dir) {
        clips_dir_problem(dir)
    } else {
        Some("a relative path")
    };
    match problem {
        None => Ok(()),
        Some(what) => Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "storage.clips_dir is {dir:?}, but it {what}: it must be empty (the clips \
                 directory under the application data directory) or an absolute path to a \
                 directory below a filesystem root"
            ),
        )),
    }
}

/// Everything the *writer* refuses.
///
/// The reader applies only the engine's own rules — see `read_effective_settings` — so
/// any value refused here is still readable: the panel can always show what the file
/// says, and refuse to write something worse.
fn validate_edit(edit: &SettingEdit) -> Result<(), CommandError> {
    match edit {
        SettingEdit::ClipsDir(dir) => validate_clips_dir(dir),
        SettingEdit::Fps(fps) => validate_fps(*fps),
        SettingEdit::OutputSize(size) => validate_output_size(size),
        SettingEdit::MicEnabled(_) | SettingEdit::AutoRecord(_) => Ok(()),
    }
}

/// Read the effective settings: the file's values where it sets them, the example's
/// where it does not.
///
/// A file that is not TOML at all is an error, like everywhere else in this module.
/// Absent keys fall back per key (recorded in `defaulted`); present-but-mistyped keys
/// are errors naming the key.
///
/// **It applies the engine's rules and nothing more.** A value the recorder loads is a
/// value this must return, however odd it looks, because the settings panel renders only
/// once this succeeds: a reader that is stricter than a start takes the only surface that
/// could repair the file away from the user. That is not hypothetical — it shipped. The
/// panel's own copy of the `output_size` rule bound the frame at `16..=16384` and trimmed
/// nothing while the engine's parser did neither, so a file carrying `" 1920x1080"`
/// recorded perfectly and left the panel permanently blank, unrepairable, and silent.
/// The panel's stricter policy therefore lives on the write path (`validate_edit`), where
/// it is a helpful refusal rather than a lockout.
pub fn read_effective_settings(path: &Path) -> Result<EffectiveSettings, CommandError> {
    let config_exists = path.is_file();
    let text = if config_exists {
        std::fs::read_to_string(path).map_err(|err| {
            CommandError::new(
                ErrorCode::Io,
                format!("could not read the config file {}: {err}", path.display()),
            )
        })?
    } else {
        // The siblings say so on this path (`StorageConfig::load`, `RecordingConfig::load`,
        // `BackgroundConfig::load`) and this one was quiet, which made "the panel is
        // showing example values" invisible in the log exactly when it matters.
        tracing::info!(
            "no config file at {}: showing the example's values",
            path.display()
        );
        EXAMPLE_CONFIG.to_string()
    };
    let doc: toml_edit::DocumentMut = text.parse().map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("the config file {} could not be parsed: {err}", path.display()),
        )
    })?;
    let example: toml_edit::DocumentMut = EXAMPLE_CONFIG.parse().map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("config.example.toml could not be parsed: {err}"),
        )
    })?;

    let mut defaulted = Vec::new();
    // A key from the file when present, else the example's, recording the fallback.
    //
    // **Only the engine's own rules run here.** A mistyped value is an error, and
    // `output_size` goes through the parser a start uses. The panel's own stricter policy
    // (the fps band, clips_dir hygiene) is deliberately absent: a reader stricter than a
    // start refuses a file the recorder runs happily, and a panel that will not render is
    // a panel that cannot repair anything.
    let clips_dir = match get_str(&doc, "storage", "clips_dir", "storage.clips_dir")? {
        Some(dir) => dir,
        None => {
            if config_exists {
                defaulted.push("storage.clips_dir");
            }
            get_str(&example, "storage", "clips_dir", "storage.clips_dir")?
                .unwrap_or_default()
        }
    };
    let fps = match get_int(&doc, "encode", "fps", "encode.fps")? {
        Some(raw) => {
            let fps: u32 = raw.try_into().map_err(|_| mistyped("encode.fps"))?;
            fps
        }
        None => {
            if config_exists {
                defaulted.push("encode.fps");
            }
            get_int(&example, "encode", "fps", "encode.fps")?
                .and_then(|raw| u32::try_from(raw).ok())
                .unwrap_or(60)
        }
    };
    let output_size = match get_str(&doc, "encode", "output_size", "encode.output_size")? {
        Some(size) => {
            validate_output_size(&size)?;
            size
        }
        None => {
            if config_exists {
                defaulted.push("encode.output_size");
            }
            get_str(&example, "encode", "output_size", "encode.output_size")?
                .unwrap_or_default()
        }
    };
    let mic_enabled = match get_bool(&doc, "mic", "enabled", "mic.enabled")? {
        Some(enabled) => enabled,
        None => {
            if config_exists {
                defaulted.push("mic.enabled");
            }
            get_bool(&example, "mic", "enabled", "mic.enabled")?.unwrap_or(false)
        }
    };
    let auto_record = match get_bool(&doc, "games", "auto_record", "games.auto_record")? {
        Some(auto) => auto,
        None => {
            if config_exists {
                defaulted.push("games.auto_record");
            }
            get_bool(&example, "games", "auto_record", "games.auto_record")?.unwrap_or(false)
        }
    };
    let watch_titles = read_watch_titles(&doc)?;

    Ok(EffectiveSettings {
        clips_dir,
        fps,
        output_size,
        mic_enabled,
        auto_record,
        watch_titles,
        defaulted,
        config_exists,
    })
}

/// Display names from `[[games.watch]]`, or the default watch list when the file names
/// none — which is what the engine watches then, too (an absent list deserialises to
/// the default, not to nothing).
fn read_watch_titles(doc: &toml_edit::DocumentMut) -> Result<Vec<String>, CommandError> {
    let Some(games) = table(doc, "games") else {
        return Ok(default_watch().iter().map(|w| w.name.clone()).collect());
    };
    let Some(watch) = games.get("watch") else {
        return Ok(default_watch().iter().map(|w| w.name.clone()).collect());
    };
    // `[[games.watch]]` is an array of tables, not an inline array: `as_array` does
    // not see it, and a `watch = "..."` string is a mistype either way.
    let tables = watch.as_array_of_tables().ok_or_else(|| mistyped("games.watch"))?;
    tables
        .iter()
        .map(|entry| {
            entry
                .get("name")
                .and_then(toml_edit::Item::as_str)
                .map(str::to_string)
                .ok_or_else(|| mistyped("games.watch"))
        })
        .collect()
}

/// Write edited keys into the file at `path`, preserving everything else.
///
/// Comments, blank lines and the formatting of untouched keys survive: the file is
/// parsed to a document, only the edited values are replaced (missing parent tables
/// are created), and the document is written back atomically. All edits are validated
/// before anything is written, and a validation failure writes nothing. A missing file
/// is created from the example first, so the write changes exactly what was asked.
/// An empty edit list is a no-op.
///
/// **This is where the panel's stricter policy belongs** (`validate_edit`): the fps band
/// and the clips_dir hygiene are refusals aimed at a user with their hand still on the
/// value, and they may be stricter than the engine exactly because the *reader* is not —
/// a value refused here has already rendered in the panel, so the user can see what they
/// are being asked to change.
///
/// The temp file is named uniquely per call. Two overlapping applies are unlikely (the
/// window serialises them behind its `busy` flag) but the runtime does dispatch invokes
/// concurrently, and a shared name would let the surviving rename publish a
/// half-overwritten document.
pub fn write_settings(path: &Path, edits: &[SettingEdit]) -> Result<(), CommandError> {
    for edit in edits {
        validate_edit(edit)?;
    }
    if edits.is_empty() {
        return Ok(());
    }

    let text = if path.is_file() {
        std::fs::read_to_string(path).map_err(|err| {
            CommandError::new(
                ErrorCode::Io,
                format!("could not read the config file {}: {err}", path.display()),
            )
        })?
    } else {
        EXAMPLE_CONFIG.to_string()
    };
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("the config file {} could not be parsed: {err}", path.display()),
        )
    })?;

    for edit in edits {
        let (table_key, value_key, section) = match edit {
            SettingEdit::ClipsDir(_) => ("storage", "clips_dir", "storage"),
            SettingEdit::Fps(_) => ("encode", "fps", "encode"),
            SettingEdit::OutputSize(_) => ("encode", "output_size", "encode"),
            SettingEdit::MicEnabled(_) => ("mic", "enabled", "mic"),
            SettingEdit::AutoRecord(_) => ("games", "auto_record", "games"),
        };
        match doc.get(table_key) {
            None => {
                doc[table_key] = toml_edit::table();
            }
            Some(item) if item.is_table() => {}
            Some(_) => {
                return Err(CommandError::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "the config file's [{section}] is not a table, so nothing was written"
                    ),
                ));
            }
        }
        let value = match edit {
            SettingEdit::ClipsDir(dir) => toml_edit::value(dir.clone()),
            SettingEdit::Fps(fps) => toml_edit::value(*fps as i64),
            SettingEdit::OutputSize(size) => toml_edit::value(size.clone()),
            SettingEdit::MicEnabled(enabled) => toml_edit::value(*enabled),
            SettingEdit::AutoRecord(auto) => toml_edit::value(*auto),
        };
        doc[table_key][value_key] = value;
    }

    // A uniquely-named temp beside the target, written, flushed to disk, then renamed over
    // it: either the new file is there whole or the old one is, never half of either.
    //
    // The name carries the pid and a counter so that overlapping applies cannot share it —
    // the window serialises its own, but the runtime does dispatch invokes concurrently.
    // `sync_all` is what makes that promise hold against a power loss rather than only
    // against a crash of this process: without it a rename can publish a zero-length file,
    // which is precisely the "half of either" this is written to avoid.
    static WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("toml.{}.{seq}.tmp", std::process::id()));
    /// The same failure for both steps, and it clears the temp on the way out: a partial
    /// file left behind is a misleading file the next run would have to explain.
    fn could_not_write(path: &Path, tmp: &Path, err: std::io::Error) -> CommandError {
        let _ = std::fs::remove_file(tmp);
        CommandError::new(
            ErrorCode::Io,
            format!("could not write the config file {}: {err}", path.display()),
        )
    }
    let mut file = std::fs::File::create(&tmp).map_err(|e| could_not_write(path, &tmp, e))?;
    std::io::Write::write_all(&mut file, doc.to_string().as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| could_not_write(path, &tmp, e))?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(|e| could_not_write(path, &tmp, e))?;
    // A settings write that leaves no trace is a support question nobody can answer;
    // this is the line that says one happened, and to which keys.
    tracing::info!(
        "settings written to {}: {}",
        path.display(),
        edits.iter().map(SettingEdit::key).collect::<Vec<_>>().join(", ")
    );
    Ok(())
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
             scratch_dir = \"\"\n\n\
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

    // -- the [mic] / [games] half --------------------------------------------------------

    #[test]
    fn the_engine_options_default_to_off_and_unwatched() {
        // Absent sections behave like the engine's own defaults: no microphone, and no
        // watcher at all (which is what `watching_games: false` reports).
        let opts = EngineOptionsConfig::from_toml("[storage]\nclips_dir = \"\"\n").unwrap();

        assert!(!opts.mic.enabled);
        assert!(!opts.games.auto_record);
        assert_eq!(opts.games.watch, default_watch());
    }

    #[test]
    fn the_engine_options_come_from_the_file_when_it_sets_them() {
        let opts = EngineOptionsConfig::from_toml(
            "[mic]\nenabled = true\n\n[games]\nauto_record = true\npoll_ms = 1000\n",
        )
        .unwrap();

        assert!(opts.mic.enabled);
        assert!(opts.games.auto_record);
        // No watch list given: the six documented defaults, which is what the engine
        // watches then too.
        assert_eq!(opts.games.watch, default_watch());
    }

    #[test]
    fn a_mistyped_engine_section_is_an_error_naming_it() {
        let err = EngineOptionsConfig::from_toml("[games]\nauto_record = \"yes\"\n").unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("[games]"), "got: {}", err.message);
    }

    // -- the effective-settings reader --------------------------------------------------

    fn settings_file() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# a comment the writer must keep\n\
             [storage]\nclips_dir = \"D:\\\\clips\"\n\n\
             [encode]\nfps = 30\noutput_size = \"1280x720\"\n\n\
             [mic]\nenabled = true\n\n\
             [games]\nauto_record = true\n\
             [[games.watch]]\nname = \"Dota 2\"\nexe = \"dota2.exe\"\n",
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn the_reader_reports_the_files_values_and_the_default_watch_list() {
        let (_dir, path) = settings_file();
        let settings = read_effective_settings(&path).unwrap();

        assert_eq!(settings.clips_dir, "D:\\clips");
        assert_eq!(settings.fps, 30);
        assert_eq!(settings.output_size, "1280x720");
        assert!(settings.mic_enabled);
        assert!(settings.auto_record);
        assert_eq!(settings.watch_titles, vec!["Dota 2"]);
        assert!(settings.defaulted.is_empty(), "the file sets everything");
        assert!(settings.config_exists);
    }

    #[test]
    fn absent_keys_fall_back_to_the_example_and_are_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[storage]\nclips_dir = \"\"\n").unwrap();

        let settings = read_effective_settings(&path).unwrap();

        assert_eq!(settings.clips_dir, "");
        assert_eq!(settings.fps, 60, "the example's capture rate");
        assert!(settings.defaulted.contains(&"encode.fps"));
        assert!(settings.defaulted.contains(&"mic.enabled"));
        assert!(settings.defaulted.contains(&"games.auto_record"));
        assert!(!settings.defaulted.contains(&"storage.clips_dir"));
        assert_eq!(
            settings.watch_titles,
            default_watch().iter().map(|w| w.name.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_missing_file_reads_the_example_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let settings =
            read_effective_settings(&dir.path().join("config.toml")).unwrap();

        assert!(!settings.config_exists);
        assert!(settings.defaulted.is_empty(), "no file, no per-key fallback to name");
        assert_eq!(settings.fps, 60);
        assert!(!settings.auto_record);
    }

    #[test]
    fn a_mistyped_key_is_an_error_naming_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[encode]\nfps = \"fast\"\n").unwrap();

        let err = read_effective_settings(&path).unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("encode.fps"), "got: {}", err.message);
    }

    #[test]
    fn the_reader_accepts_every_value_the_recorder_accepts() {
        // A reader stricter than the engine is a settings panel that never renders — and
        // with no panel there is no way to repair the file from the UI, so the one failure
        // the user cannot fix is the one the reader invents. `resolve_dir` takes
        // `[storage] clips_dir` exactly as written and nothing anywhere bounds `fps`, so a
        // relative directory and `fps = 1000` are both values a start accepts.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[storage]\nclips_dir = \"clips\"\n\n\
             [encode]\nfps = 1000\noutput_size = \" 1920 x 1080\"\n",
        )
        .unwrap();

        let settings = read_effective_settings(&path).unwrap();

        assert_eq!(settings.fps, 1000, "the engine bounds nothing");
        assert_eq!(settings.clips_dir, "clips", "the engine takes the string as given");
        assert_eq!(settings.output_size, " 1920 x 1080", "the engine's parser trims");
    }

    #[test]
    fn the_reader_still_refuses_an_output_size_the_recorder_refuses() {
        // The one key the engine *does* validate, so the reader keeps refusing it: the
        // panel must not show a value a start would reject.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[encode]\noutput_size = \"wide\"\n").unwrap();

        let err = read_effective_settings(&path).unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("encode.output_size"), "got: {}", err.message);
    }

    #[test]
    fn a_mistyped_watch_list_says_what_it_should_have_been() {
        // `games.watch` is an array of tables. Reporting it as "not a true/false value"
        // names the wrong shape and sends the reader looking at the wrong key.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[games]\nwatch = \"Dota 2\"\n").unwrap();

        let err = read_effective_settings(&path).unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("games.watch"), "got: {}", err.message);
        assert!(
            !err.message.contains("true/false"),
            "the message must not name a bool: {}",
            err.message
        );
    }

    // -- the writer ---------------------------------------------------------------------

    #[test]
    fn the_writer_changes_only_what_it_is_asked_and_keeps_the_comments() {
        let (_dir, path) = settings_file();
        let before = std::fs::read_to_string(&path).unwrap();

        write_settings(
            &path,
            &[SettingEdit::Fps(60), SettingEdit::AutoRecord(false)],
        )
        .unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("# a comment the writer must keep"), "comments survive");
        assert!(after.contains("fps = 60"));
        assert!(after.contains("auto_record = false"));
        assert!(after.contains("clips_dir = \"D:\\\\clips\""), "untouched keys survive");
        assert!(after.contains("output_size = \"1280x720\""));
        assert!(after.contains("enabled = true"));
        assert!(
            after.lines().count() <= before.lines().count() + 1,
            "no reformatting sprawl:\n{after}"
        );
        // And the file still reads back as what was written.
        let settings = read_effective_settings(&path).unwrap();
        assert_eq!(settings.fps, 60);
        assert!(!settings.auto_record);
    }

    #[test]
    fn the_writer_creates_missing_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[storage]\nclips_dir = \"\"\n").unwrap();

        write_settings(&path, &[SettingEdit::MicEnabled(true)]).unwrap();

        let settings = read_effective_settings(&path).unwrap();
        assert!(settings.mic_enabled);
        assert!(settings.defaulted.contains(&"encode.fps"), "nothing else was invented");
    }

    #[test]
    fn the_writer_creates_a_missing_file_from_the_example() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        write_settings(&path, &[SettingEdit::OutputSize("1920x1080".to_string())]).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("output_size = \"1920x1080\""));
        assert!(text.contains("[storage]"), "the example's sections came along");
        let settings = read_effective_settings(&path).unwrap();
        assert!(settings.config_exists);
        assert_eq!(settings.output_size, "1920x1080");
    }

    /// Everything in the config's directory except the config itself — i.e. any temp file
    /// a write failed to clear.
    fn leftovers(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "config.toml")
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_rejected_edit_writes_nothing() {
        let (dir, path) = settings_file();
        let before = std::fs::read_to_string(&path).unwrap();

        let err = write_settings(
            &path,
            &[SettingEdit::Fps(60), SettingEdit::Fps(0)],
        )
        .unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("encode.fps"), "got: {}", err.message);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "all or nothing");
        assert_eq!(
            leftovers(dir.path()),
            Vec::<String>::new(),
            "no temp file is left behind"
        );
    }

    #[test]
    fn a_write_that_worked_leaves_no_temp_file_either() {
        // The temp name carries the pid and a counter now, so it is not the fixed
        // `config.toml.tmp` a test could guess by name — it has to be looked for.
        let (dir, path) = settings_file();

        write_settings(&path, &[SettingEdit::Fps(60)]).unwrap();

        assert_eq!(leftovers(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn an_empty_edit_list_is_a_no_op() {
        let (_dir, path) = settings_file();
        let before = std::fs::read_to_string(&path).unwrap();

        write_settings(&path, &[]).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn a_section_that_is_not_a_table_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "encode = 5\n").unwrap();

        let err = write_settings(&path, &[SettingEdit::Fps(30)]).unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("[encode]"), "got: {}", err.message);
    }

    #[test]
    fn the_fps_band_is_the_panels_own_and_applies_only_when_writing() {
        // Nothing in the engine bounds `fps`, so this band is the panel's ergonomics, not a
        // shared rule — which is why it lives on the write path, where the user's hand is
        // still on the value, and not on the reader above, where a bound would refuse a
        // file the recorder runs.
        for bad in [0, 241, 1000] {
            let err = validate_edit(&SettingEdit::Fps(bad)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidInput, "fps {bad}");
            assert!(err.message.contains("encode.fps"), "got: {}", err.message);
        }
        assert!(validate_edit(&SettingEdit::Fps(1)).is_ok());
        assert!(validate_edit(&SettingEdit::Fps(240)).is_ok());
    }

    #[test]
    fn clips_dir_accepts_empty_or_an_absolute_directory_with_something_in_it() {
        assert!(validate_clips_dir("").is_ok(), "empty resets to the default");
        assert!(validate_clips_dir("/media/clips").is_ok());
        assert!(validate_clips_dir("C:\\clips").is_ok());
        assert!(validate_clips_dir("C:/clips").is_ok());
        assert!(validate_clips_dir("\\\\server\\share\\clips").is_ok());

        for bad in ["clips", "localplay/clips", "C:clips"] {
            assert!(
                validate_clips_dir(bad).is_err(),
                "{bad:?} is relative and must be refused"
            );
        }
    }

    #[test]
    fn clips_dir_is_refused_when_it_would_widen_the_asset_scope_to_a_whole_volume() {
        // The directory is handed to `allow_directory(.., true)` — the webview's only read
        // channel — so a root, a share that names nothing inside itself, or a path that
        // climbs back out would expose far more than the clips the user meant to share.
        for bad in [
            "/",
            "\\",
            "C:\\",
            "C:/",
            "\\\\server",
            "\\\\server\\share",
            "C:\\clips\\..\\..\\Windows",
            "/media/../etc",
        ] {
            let err = validate_clips_dir(bad).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidInput, "{bad:?} must be refused");
            assert!(err.message.contains("storage.clips_dir"), "got: {}", err.message);
        }
    }

    #[test]
    fn output_size_follows_the_engines_own_parser() {
        // Reused rather than re-invented: `localplay_recorder::parse_output_size` trims
        // each component and sets no bounds, so the panel accepts exactly what a start
        // accepts. The copy that used to live here did neither, which is how
        // `output_size = " 1920x1080"` came to record fine while `get_settings` refused it.
        assert!(validate_output_size("").is_ok(), "empty is the native size");
        assert!(validate_output_size("1920x1080").is_ok());
        assert!(validate_output_size("640x480").is_ok());
        assert!(validate_output_size("1920 x 1080").is_ok(), "the engine trims");
        assert!(validate_output_size("99999x1080").is_ok(), "the engine bounds nothing");

        for bad in ["wide", "1920x", "x1080", "1920x1080x2"] {
            assert!(
                validate_output_size(bad).is_err(),
                "{bad:?} must be refused"
            );
        }
    }
}
