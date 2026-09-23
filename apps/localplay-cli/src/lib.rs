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
//!
//! # Taking a clip without touching the keyboard
//!
//! `Ctrl+F8` is the shipping trigger, and it stays that way. It is also the reason the clip
//! path could never be verified from a script: pressing it requires synthesising input, and
//! a machine running kernel-level anti-cheat treats synthetic input as hostile — so the
//! verification that matters most was the one that could not safely be run.
//!
//! `buffer --self-test-clip-after <SECONDS>` is the way out. It is a *diagnostic*, not a
//! feature: it waits until the ring holds a full window, calls the same
//! [`Recorder::clip_now`] the hotkey calls (once), and stops. No keyboard or mouse input is
//! sent, simulated or injected anywhere in this crate, and nothing enumerates windows; the
//! only trigger is an in-process function call on the engine's own media-time path. The
//! lines it produces are the hotkey's own lines, so the runbook's criteria read the same
//! way in either mode.

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

/// The `buffer` subcommand's own help text (see `main.rs`).
///
/// It documents `--self-test-clip-after` next to the flags a user may actually pass, and
/// says what the flag does *not* do, because "does this touch my input?" is the first
/// question a diagnostic that takes a clip has to answer.
pub fn help() -> String {
    let mut text = String::from(
        "\
localplay — Phase 1 headless replay buffer

usage:
  localplay-cli buffer [OPTIONS]

options:
  --self-test-clip-after <SECONDS>
        Verification aid, not an end-user feature. Once the ring holds SECONDS of
        media — and never before it holds a full pre-roll plus post-roll — take
        exactly one clip through the same media-time trigger the clip hotkey
        uses, log what that path logs (the trigger instant with its media time,
        wall time and drift, then the written clip), and stop. The clip hotkey
        remains the shipping mechanism; this flag is an *additional* trigger.

        It sends, simulates and injects no keyboard or mouse input, and it does
        not enumerate windows. It exists so that a machine whose anti-cheat
        treats synthetic input as hostile can still have its clip path verified.
  --dev-software-encoder
        Only in builds carrying the `test-encoders` feature (never a release
        build): use libx264 instead of a GPU hardware encoder, for pipeline
        smoke tests on a host with no GPU encoder.

configuration:
  %LOCALAPPDATA%\\localplay\\config.toml on Windows (the directory
  $LOCALAPPDATA points at may be redirected for a scratch run); the example
  file is `config.example.toml`. localplay never reads configuration from
  environment variables.
",
    );
    // The hotkey is part of the runtime, not of the flag list; naming the configured
    // chord here would mean reading the config for `--help`, which the flag parser does
    // not do.
    text.push_str("\nThe clip hotkey (config `[hotkeys] clip`, default Ctrl+F8) is unchanged.\n");
    text
}

/// Options for the `buffer` subcommand, from the command line (and the environment for the
/// data directory only).
#[derive(Debug, Clone)]
pub struct BufferOptions {
    /// The application data directory: `config.toml`, `scratch/`, `clips/` and the index.
    /// `app_data_dir()` by default; a verification harness points it somewhere disposable.
    pub app_dir: PathBuf,
    /// `--self-test-clip-after <SECONDS>`. `None` is the shipping behaviour.
    pub self_test_clip_after: Option<u64>,
    /// `--dev-software-encoder`.
    pub dev_software_encoder: bool,
}

/// Parse the flags the `buffer` subcommand accepts (everything after `buffer`).
///
/// Strict on purpose: an unrecognised argument is an error rather than something quietly
/// ignored, because the one thing this parser exists for is a verification flag that must
/// provably have been understood.
pub fn parse_buffer_args(args: &[String]) -> Result<(Option<u64>, bool)> {
    let mut self_test_clip_after = None;
    let mut dev_software_encoder = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let value_of = |it: &mut std::slice::Iter<'_, String>, flag: &str| -> Result<String> {
            it.next()
                .cloned()
                .with_context(|| format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "--self-test-clip-after" => {
                let value = value_of(&mut it, "--self-test-clip-after")?;
                self_test_clip_after = Some(parse_self_test_seconds(&value)?);
            }
            "--dev-software-encoder" => dev_software_encoder = true,
            other => match other.strip_prefix("--self-test-clip-after=") {
                Some(value) => self_test_clip_after = Some(parse_self_test_seconds(value)?),
                None => bail!("unexpected argument {other:?}\n\n{}", help()),
            },
        }
    }
    Ok((self_test_clip_after, dev_software_encoder))
}

fn parse_self_test_seconds(value: &str) -> Result<u64> {
    let seconds: u64 = value.parse().with_context(|| {
        format!("--self-test-clip-after expects whole seconds, e.g. 45 (got {value:?})")
    })?;
    if seconds == 0 {
        bail!("--self-test-clip-after must be at least 1 second");
    }
    Ok(seconds)
}

/// How much media the ring must hold before a self-test clip is taken, in ms.
///
/// The requested hold (`--self-test-clip-after`) is a floor, not the whole rule: a clip
/// spliced before the pre-roll is complete would be short and would say nothing about the
/// configured window, so the wait is never shorter than one full pre-roll plus post-roll.
/// Pure, so the rule is pinned by a unit test rather than by reading the wait loop.
pub fn self_test_need_ms(after_seconds: u64, pre_seconds: u64, post_seconds: u64) -> u64 {
    (after_seconds * 1000).max((pre_seconds + post_seconds) * 1000)
}

/// The `buffer` subcommand, as `main.rs` calls it: flags from the process's own arguments.
pub fn run_buffer() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let (self_test_clip_after, dev_software_encoder) = parse_buffer_args(&args)?;
    run_buffer_with(BufferOptions {
        app_dir: app_data_dir(),
        self_test_clip_after,
        dev_software_encoder,
    })
}

/// The `buffer` subcommand: capture → encode → segment ring → hotkey → clip.
///
/// Split from [`run_buffer`] so an integration test can drive the whole command in-process
/// with its own data directory — the self-test trigger has to be reachable from a test, or
/// it is the same kind of unverifiable code the hotkey path has always been.
pub fn run_buffer_with(opts: BufferOptions) -> Result<()> {
    let app_dir = opts.app_dir;
    let cfg_path = app_dir.join("config.toml");
    let cfg = if cfg_path.is_file() {
        Config::load(&cfg_path)?
    } else {
        Config::from_toml(include_str!("../../../config.example.toml"))?
    };
    let Config { buffer, encode, storage, hotkeys, events, .. } = cfg;
    // Read before the move: these are the CLI's own line (it is logged after the engine is
    // already capturing). The rate the line prints comes from the engine rather than from
    // `encode.fps`, because the engine may have measured that this machine cannot hold the
    // configured rate and is pacing to a lower one — see `RecorderStatus::effective_fps`.
    let (pre_seconds, post_seconds) = (buffer.pre_seconds, buffer.post_seconds);

    let bin = FfmpegBinaries::discover(None)?;
    let hotkey = localplay_events::hotkey::Hotkey::parse(&hotkeys.clip)?;
    // The event sources need the application data directory too (the GSI token and the
    // generated cfg file live there), and the recorder takes it by value below.
    let data_dir = app_dir.clone();

    // `--dev-software-encoder` only exists when built with the test-encoders feature; it
    // selects libx264, needs no GPU vendor at all, and is never reachable from the config
    // file. The engine applies the gate (see `localplay_recorder::resolve_encoder`).
    let dev_software = opts.dev_software_encoder;

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
    // always was. The chord printed is the *parsed* one (its `Display`), so a config that
    // says `ctrl+f8` reads back as `Ctrl+F8` and matches what the desktop shell shows for
    // the same file. The rate is the one the pipeline is running at: when it is below the
    // configured `encode.fps`, the engine has already logged a warning saying what it
    // measured and why (see `log_rate_decision` in `localplay-recorder`).
    tracing::info!(
        "buffering {}s pre / {}s post at {}fps; press {} to clip",
        pre_seconds,
        post_seconds,
        recorder.status().effective_fps,
        hotkey
    );

    // The game-event sources. Started after the recorder (a source cannot ask for a clip
    // before there is anything to clip) and before the hotkey, so that an event arriving in
    // the first moments is not lost to a wait that is already running.
    //
    // Self-test mode does not start them: the self-test takes **exactly one** clip, and a
    // source that asked for one of its own would break that guarantee (as well as binding a
    // port or polling a game on a run that exists to touch nothing but its own capture).
    let sources = match opts.self_test_clip_after {
        Some(_) => {
            tracing::info!(
                "self-test mode: the game-event sources are not started (the run takes \
                 exactly one clip)"
            );
            let (_sink, events) = mpsc::channel();
            EventSources { events, _lol: None, _gsi: None }
        }
        None => start_event_sources(&data_dir, &events),
    };

    // The hotkey listener is installed in both modes: it is part of the shipping startup
    // path, and self-test mode must not be a different program. What self-test mode does
    // *not* do is read it — the trigger below is the only thing that takes a clip.
    //
    // `listen` fails loudly when the chord cannot be registered (it may be held by another
    // application, or by the desktop app running against this same `config.toml`), and a
    // buffer run with a hotkey that silently does nothing is not worth starting: the
    // context below is what turns that into an error the user can read.
    let hotkeys = hotkey::listen(hotkey).context("installing the clip hotkey")?;

    // The verification-only trigger (`--self-test-clip-after`). It never synthesises input:
    // it is an in-process call to the same media-time trigger the hotkey drives.
    if let Some(after_seconds) = opts.self_test_clip_after {
        return self_test_clip(&recorder, after_seconds, pre_seconds, post_seconds);
    }

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

/// The verification-only self-test trigger: wait for a full window, take one clip, stop.
///
/// This is the whole reason the flag exists. A `Ctrl+F8` press is the shipping way to take
/// a clip, but pressing it requires *synthesising input* on a machine whose anti-cheat
/// treats that as hostile — and a verification that gets the machine banned is not
/// verification. So the same trigger is reachable from inside the process:
///
/// * the wait is on the ring's media timeline (`span_ms`), the clock the trigger itself
///   uses, and it is never shorter than a full pre-roll plus post-roll
///   ([`self_test_need_ms`]);
/// * the trigger is [`Recorder::clip_now`] — the hotkey's own call, not a copy of it, so
///   the trigger line (`hotkey pressed: media=… wall=… (drift …ms)`) and the `wrote …`
///   line are exactly the lines the runbook's criteria 3–5 and 8 already read;
/// * nothing here sends, simulates or injects a keyboard or mouse event, and nothing
///   enumerates windows ([`Recorder`] is the only thing touched).
///
/// After the clip is written and indexed the recorder is stopped and the process exits
/// with the clip's own outcome as its exit code, which is what makes the run scriptable.
fn self_test_clip(
    recorder: &Recorder,
    after_seconds: u64,
    pre_seconds: u64,
    post_seconds: u64,
) -> Result<()> {
    let need_ms = self_test_need_ms(after_seconds, pre_seconds, post_seconds);
    tracing::info!(
        "self-test: one clip will be taken once the ring holds {need_ms}ms of media \
         (--self-test-clip-after {after_seconds}s, at least {pre_seconds}s pre + \
         {post_seconds}s post); no keyboard or mouse input is sent, simulated or injected, \
         and no window is enumerated"
    );

    loop {
        let span_ms = recorder.status().span_ms;
        if span_ms >= need_ms {
            tracing::info!("self-test: the ring holds {span_ms}ms of media; taking the clip");
            break;
        }
        if !recorder.is_running() {
            // The engine stopped on its own and kept the reason in its status.
            recorder.stop()?;
            bail!("the recorder stopped while the self-test was waiting for {need_ms}ms of media");
        }
        std::thread::sleep(HOTKEY_POLL);
    }

    if let Err(err) = recorder.clip_now() {
        let _ = recorder.stop();
        return Err(err.context("taking the self-test clip"));
    }
    // Flush the encoder and close the capture session before exiting, so the process's
    // exit code is decided by the engine's own shutdown and not by a drop().
    recorder.stop()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_buffer_flags_default_to_the_shipping_behaviour() {
        // No flags: nothing about the hotkey path changes, and the self-test trigger is
        // not armed. This is the case a user runs.
        let (self_test, dev) = parse_buffer_args(&[]).expect("no flags is a valid command line");
        assert_eq!(self_test, None);
        assert!(!dev);
    }

    #[test]
    fn the_self_test_flag_parses_seconds_in_either_form() {
        assert_eq!(
            parse_buffer_args(&args(&["--self-test-clip-after", "45"])).expect("spaced form").0,
            Some(45)
        );
        assert_eq!(
            parse_buffer_args(&args(&["--self-test-clip-after=45"])).expect("= form").0,
            Some(45)
        );
        // The two forms are one setting, not two: the last one wins, and an unrelated flag
        // is unaffected.
        assert_eq!(
            parse_buffer_args(&args(&[
                "--self-test-clip-after=10",
                "--dev-software-encoder",
                "--self-test-clip-after",
                "20",
            ]))
            .expect("last one wins"),
            (Some(20), true)
        );
    }

    #[test]
    fn a_bad_self_test_value_is_refused_rather_than_defaulted() {
        // A verification flag that silently did nothing would be worse than a crash: the
        // run would look like it verified the trigger when it never armed it.
        for bad in [
            args(&["--self-test-clip-after"]),
            args(&["--self-test-clip-after", "0"]),
            args(&["--self-test-clip-after", "five"]),
            args(&["--self-test-clip-after=-1"]),
        ] {
            let err = parse_buffer_args(&bad)
                .expect_err("a bad value must be refused, never defaulted")
                .to_string();
            assert!(
                err.contains("self-test-clip-after"),
                "the error must name the flag, got: {err}"
            );
        }
    }

    #[test]
    fn an_unknown_flag_is_an_error_that_prints_the_help() {
        let err = parse_buffer_args(&args(&["--clip-after", "5"]))
            .expect_err("an unknown flag must be refused")
            .to_string();
        assert!(err.contains("--clip-after"), "must name the bad argument: {err}");
        assert!(err.contains("usage:"), "must show the usage: {err}");
    }

    #[test]
    fn the_help_documents_that_no_input_is_synthesised() {
        // The flag's whole justification. If this sentence is ever dropped, the reason the
        // flag exists has been lost with it.
        let help = help();
        assert!(help.contains("--self-test-clip-after"));
        assert!(help.contains("no keyboard or mouse input"));
        assert!(help.contains("not enumerate windows"));
    }

    #[test]
    fn the_self_test_wait_is_never_shorter_than_the_configured_window() {
        // Requested hold above the window: the request wins.
        assert_eq!(self_test_need_ms(45, 5, 3), 45_000);
        // Requested hold below it: a full pre-roll plus post-roll is the floor, because a
        // clip spliced before the pre-roll is complete would not be the configured window.
        assert_eq!(self_test_need_ms(2, 5, 3), 8_000);
        assert_eq!(self_test_need_ms(8, 5, 3), 8_000);
    }
}
