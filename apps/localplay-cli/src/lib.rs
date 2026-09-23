//! The localplay CLI — the headless front-end over the recording engine.
//!
//! This crate is a *driver*, and nothing else. The pipeline (capture → audio → encode →
//! segment ring → clip) lives in `localplay-recorder`, which the desktop shell drives
//! too; what is left here is what only a headless front-end needs:
//!
//! 1. parse the config file (the CLI owns `[hotkeys]` and `[events]`; the recorder's
//!    `[buffer]`/`[encode]`/`[storage]` sections are shared types),
//! 2. start a [`Recorder`],
//! 3. start the game-event sources `[events]` enables, and install the global hotkey,
//! 4. turn a press or a derived event into a clip through **one** call
//!    ([`Recorder::clip_now_with`]), and stop on the way out.
//!
//! # The two event sources, and what they are allowed to be
//!
//! `[events] lol_poll_enabled` starts the League Live Client poller (spec §7.1);
//! `[events] gsi_port` (non-zero) starts the CS2 / Dota 2 GSI listener (spec §7.2). Both are
//! loopback-only by construction — see the `localplay-events` crate, and the guard test that
//! checks it statically — and both report what they derive over one channel, which this loop
//! drains. A source that fails to start is reported and skipped: a machine with no League
//! install must still record on the hotkey.
//!
//! What the loop does with an event is the only policy here: a *highlight*
//! ([`localplay_events::EventKind::is_highlight`] — a kill, a bomb, the end of a game) takes a clip through the
//! hotkey's own trigger, and a *marker* (a game or round boundary) is recorded in the
//! `events` table without one. Neither is a second trigger mechanism: the clip is
//! [`Recorder::clip_now_with`], the same media-time path with a reason attached.
//!
//! # Why the engine moved out of this file
//!
//! Everything the PoC did used to live here, for a reason that had already been paid for
//! once: the hotkey's post-roll wait was an inline loop in `main`, no test could reach it,
//! and it shipped broken (the loop never fed the encoder, so every trigger timed out).
//! Code that no test can call is code that cannot be verified.
//!
//! That argument still holds, and it now points at a crate rather than at this library:
//! the engine is reachable from the recorder's own tests *and* from the desktop shell —
//! the application a user actually runs, which could browse, trim and delete clips but
//! could not record one. Nothing in this file re-implements any of it.
//!
//! # What this loop does not do
//!
//! It does not log the status line. `frames= segments= bytes= span= …` is emitted by the
//! engine, on the thread that owns the counters, so the same line is observable whichever
//! front-end started the recording; a second copy here would be a second source of truth.
//! What this loop owns is the one thing the engine deliberately does not: the hotkey.

pub mod config;

use anyhow::{bail, Context, Result};
use config::{Config, EventsSection};
use localplay_events::gsi::{self, GsiConfig, GsiListener};
use localplay_events::lol::{self, LolConfig, LolHandle};
use localplay_events::{hotkey, GameEvent};
use localplay_media::FfmpegBinaries;
use localplay_recorder::{ClipReason, Recorder, RecorderConfig, Sources};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

/// How long the hotkey wait blocks before the loop looks at the recorder again.
///
/// Small enough that a trigger and a shutdown are both noticed promptly (the engine's own
/// work happens on its thread, so nothing here is on the clip path), and large enough that
/// a platform with no hotkey listener — everything off Windows — sleeps in this loop
/// rather than spinning in it: there `wait_for_press` returns immediately, and the sleep
/// below is what keeps the process idle.
const HOTKEY_POLL: Duration = Duration::from_millis(10);

/// The `buffer` subcommand: capture → encode → segment ring → hotkey → clip.
pub fn run_buffer() -> Result<()> {
    let app_dir = app_data_dir();
    let cfg_path = app_dir.join("config.toml");
    let cfg = if cfg_path.is_file() {
        Config::load(&cfg_path)?
    } else {
        Config::from_toml(include_str!("../../../config.example.toml"))?
    };
    let Config { buffer, encode, storage, hotkeys, events, .. } = cfg;
    // Read before the move: this line is the CLI's own, and it is logged after the engine
    // is already capturing.
    let (pre_seconds, post_seconds, fps) =
        (buffer.pre_seconds, buffer.post_seconds, encode.fps);

    let bin = FfmpegBinaries::discover(None)?;
    let hotkey = localplay_events::hotkey::Hotkey::parse(&hotkeys.clip)?;
    // The event sources need the application data directory too (the GSI token and the
    // generated cfg file live there), and the recorder takes it by value below.
    let data_dir = app_dir.clone();

    // `--dev-software-encoder` only exists when built with the test-encoders feature; it
    // selects libx264, needs no GPU vendor at all, and is never reachable from the config
    // file. The engine applies the gate (see `localplay_recorder::resolve_encoder`).
    let dev_software = std::env::args().any(|a| a == "--dev-software-encoder");

    let recorder = Recorder::start(RecorderConfig {
        bin,
        app_data_dir: app_dir,
        buffer,
        encode,
        storage,
        // WGC + WASAPI on Windows, the synthetic stubs everywhere else
        // (`localplay_capture::platform` decides, and on Windows it refuses to fall back
        // to a stub).
        sources: Sources::Platform,
        dev_software_encoder: dev_software,
    })?;

    // From here the engine is capturing: the encoder is spawned, the ring is built and
    // adopted, and the capture session is open. This line is the CLI's own — the hotkey is
    // what only it knows about — and it is emitted at the same point in the sequence it
    // always was.
    tracing::info!(
        "buffering {}s pre / {}s post at {}fps; press {} to clip",
        pre_seconds,
        post_seconds,
        fps,
        hotkeys.clip
    );

    // The game-event sources. Started after the recorder (a source cannot ask for a clip
    // before there is anything to clip) and before the hotkey, so that an event arriving in
    // the first moments is not lost to a wait that is already running.
    let sources = start_event_sources(&data_dir, &events);

    let hotkeys = hotkey::listen(hotkey)?;

    // The driver loop. Everything below either waits for a press, hands the trigger to the
    // engine, or leaves — and every path out of it stops the recorder, which flushes the
    // encoder and closes the capture session.
    loop {
        if hotkey::wait_for_press(&hotkeys, HOTKEY_POLL) {
            if let Err(err) = recorder.clip_now() {
                // The engine logs what went wrong; stopping first means the encoder is
                // flushed and the capture session closed before the process gives up.
                let _ = recorder.stop();
                return Err(err.context("taking a clip"));
            }
        } else {
            std::thread::sleep(HOTKEY_POLL);
        }

        // Anything the integrations derived, one at a time. A highlight is a clip through
        // the same call the hotkey just made; a marker is recorded and takes no footage.
        // `try_recv` rather than `recv`, so a quiet game cannot make the hotkey wait.
        while let Ok(event) = sources.events.try_recv() {
            if let Err(err) = act_on_event(&recorder, event) {
                let _ = recorder.stop();
                return Err(err);
            }
        }

        if !recorder.is_running() {
            // The engine stopped on its own — a failed capture, a scratch cap violation —
            // and it kept the reason in its status. `stop()` reports it.
            recorder.stop()?;
            bail!("the recorder stopped; see the log above");
        }
    }
}

/// The event sources a front-end was configured to run, and the channel they report on.
///
/// Dropping this stops the sources: each handle joins its own thread (see the handles'
/// `Drop`), so nothing keeps polling a game or a port after the driver loop has ended. The
/// handles are held here rather than in the loop for exactly that reason.
struct EventSources {
    /// Derived events, in the order the sources produced them.
    events: Receiver<GameEvent>,
    /// Kept alive (not read): dropping it stops the poller.
    _lol: Option<LolHandle>,
    /// The same for the GSI listener.
    _gsi: Option<GsiListener>,
}

/// Start whatever `[events]` asked for, best effort.
///
/// Each source is independent and none is essential: a League poll is pointless on a machine
/// with no League install, and a GSI listener needs a port and a token file, either of which
/// can fail. A failure is a warning naming the setting, never a reason to refuse to record —
/// the hotkey still works, and that is the fallback spec §7.4 describes.
fn start_event_sources(app_dir: &Path, events: &EventsSection) -> EventSources {
    let (sink, events_rx) = mpsc::channel::<GameEvent>();

    let lol = if events.lol_poll_enabled {
        match lol::spawn(LolConfig::live_client(), sink.clone()) {
            Ok(handle) => {
                tracing::info!(
                    "League of Legends: polling {} for game events (set \
                     events.lol_poll_enabled = false to stop)",
                    handle.endpoint().url()
                );
                Some(handle)
            }
            Err(err) => {
                tracing::warn!("events.lol_poll_enabled is set but polling could not start: {err:#}");
                None
            }
        }
    } else {
        tracing::info!("League of Legends polling is disabled (events.lol_poll_enabled = false)");
        None
    };

    let gsi = if events.gsi_port == 0 {
        tracing::info!("the CS2 / Dota 2 listener is disabled (events.gsi_port = 0)");
        None
    } else {
        match start_gsi(app_dir, events.gsi_port, sink.clone()) {
            Ok(listener) => Some(listener),
            Err(err) => {
                tracing::warn!("events.gsi_port is set but the listener could not start: {err:#}");
                None
            }
        }
    };

    EventSources { events: events_rx, _lol: lol, _gsi: gsi }
}

/// Bring up the GSI listener and write out the file the user has to install (spec §7.2).
///
/// The application never writes into a game's installation directory: it generates the file
/// into its own data directory and says where to copy it. The token is loaded from (or
/// generated into) the data directory, so the file the user installed once keeps working
/// across runs — and it is never logged, only the path it lives at.
fn start_gsi(app_dir: &Path, port: u16, sink: localplay_events::EventSink) -> Result<GsiListener> {
    let token_path = app_dir.join("gsi-token.txt");
    let token = gsi::load_or_create_token(&token_path)?;

    // One file per game, in its own directory: the two games subscribe different data
    // blocks, and the file name has to be the same for both, so keeping them apart is the
    // only way not to hand the user the wrong file.
    let dirs = [
        ("cs2", gsi::integration_cfg(port, &token), "Counter-Strike 2 / CS:GO"),
        ("dota2", gsi::integration_cfg_dota(port, &token), "Dota 2"),
    ];
    for (game, text, name) in dirs {
        let dir = app_dir.join("gsi").join(game);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("gamestate_integration_localplay.cfg");
        std::fs::write(&path, text)
            .with_context(|| format!("writing {}", path.display()))?;
        tracing::info!(
            "{name}: copy {} into the game's cfg directory and restart the game",
            path.display()
        );
    }

    let listener = gsi::spawn(GsiConfig::new(port, token), sink)?;
    tracing::info!(
        "the token the game must carry is in {} (never logged, and not in the config file)",
        token_path.display()
    );
    Ok(listener)
}

/// Do what a derived event asks for.
///
/// The only decision here is highlight versus marker, and it is the vocabulary's
/// ([`localplay_events::EventKind::is_highlight`]) rather than this loop's. Everything else is one call.
fn act_on_event(recorder: &Recorder, event: GameEvent) -> Result<()> {
    if event.is_highlight() {
        let described = format!("{} {}", event.source, event.kind);
        recorder
            .clip_now_with(ClipReason::GameEvent(event))
            .map(|clip| {
                tracing::info!("the {described} clip is {}", clip.metadata.path.display());
            })
            .with_context(|| format!("taking a clip for a {described} event"))
    } else {
        let described = format!("{} {}", event.source, event.kind);
        recorder
            .note_event(event)
            .map(|id| tracing::info!("recorded the {described} marker as event #{id}"))
            .with_context(|| format!("recording a {described} marker"))
    }
}

fn app_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("localplay")
}
