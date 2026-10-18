//! The localplay CLI: the headless front-end over the recording engine.
//!
//! A driver and nothing else. The pipeline (capture, audio, encode, segment ring, clip) lives in
//! `localplay-recorder`, which the desktop shell drives too; this crate only:
//!
//! 1. parses the config file (the CLI owns `[hotkeys]` and `[events]`; the recorder's
//!    `[buffer]`/`[encode]`/`[storage]` sections are shared types),
//! 2. starts a [`Recorder`],
//! 3. starts the game-event sources `[events]` enables, and installs the global hotkey,
//! 4. turns a press or a derived event into a clip through one call
//!    ([`Recorder::clip_now_with`]), and stops on the way out.
//!
//! # The two event sources
//!
//! `[events] lol_poll_enabled` starts the League Live Client poller; `[events] gsi_port`
//! (non-zero) starts the CS2 / Dota 2 GSI listener. Both are loopback-only by construction (see
//! `localplay-events` and its static guard test) and report over one channel, which this loop
//! drains. A source that fails to start is reported and skipped: a machine with no League
//! install must still record on the hotkey.
//!
//! The loop's only policy: a highlight ([`localplay_events::EventKind::is_highlight`]: a kill, a
//! bomb, the end of a game) takes a clip through the hotkey's own trigger, and a marker (a game
//! or round boundary) is recorded in the `events` table without one. Neither is a second trigger
//! mechanism: the clip is always [`Recorder::clip_now_with`].
//!
//! # What this loop does not do
//!
//! It does not log the status line (`frames= segments= bytes= span= ...`). The engine emits it
//! on the thread that owns the counters, so it reads the same whichever front-end started the
//! recording. What this loop owns is the one thing the engine does not: the hotkey.
//!
//! # Taking a clip without touching the keyboard
//!
//! `Ctrl+F8` is the shipping trigger, but pressing it from a script means synthesising input,
//! which a machine running kernel-level anti-cheat treats as hostile.
//!
//! `buffer --self-test-clip-after <SECONDS>` is a diagnostic, not a feature: it waits until the
//! ring holds a full window, calls the same [`Recorder::clip_now`] the hotkey calls (once), and
//! stops. No keyboard or mouse input is sent, simulated or injected anywhere in this crate and
//! nothing enumerates windows; the trigger is an in-process call on the engine's media-time path,
//! and it produces the hotkey's own log lines.

pub mod config;

use anyhow::{bail, Context, Result};
use config::Config;
#[cfg(feature = "test-encoders")]
use localplay_capture::stub::StubConfig;
use localplay_events::hotkey;
use localplay_media::FfmpegBinaries;
use localplay_recorder::event_driver::{act_on_burst, start_event_sources, EventSources, Policy};
#[cfg(feature = "test-encoders")]
use localplay_recorder::STUB_CAPTURE_SIZE;
use localplay_recorder::{Recorder, RecorderConfig, RecorderOptions, RecordingMode, Sources};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

/// How long the hotkey wait blocks before the loop looks at the recorder again.
///
/// Short enough that a trigger and a shutdown are noticed promptly, long enough that a platform
/// with no hotkey listener (everything off Windows, where `wait_for_press` returns immediately)
/// sleeps here instead of spinning.
const HOTKEY_POLL: Duration = Duration::from_millis(10);

/// The `buffer` subcommand's own help text (see `main.rs`).
///
/// It documents `--self-test-clip-after` beside the flags a user may pass, and says what the
/// flag does not do: "does this touch my input?" is the first question such a diagnostic must
/// answer.
pub fn help() -> String {
    let mut text = String::from(
        "\
localplay — Phase 1 headless replay buffer

usage:
  localplay-cli buffer [OPTIONS]

options:
  --mode <buffer|session>
        Which recording to make, overriding the file's `[recorder] mode`.
          buffer  — the rolling replay buffer: a bounded scratch ring, a clip per
                    hotkey press. The default, and what every earlier build did.
          session — record the whole session, then concatenate it losslessly into
                    one `session-<timestamp>.mp4` under the sessions area when the
                    recording stops. The scratch cap does not apply to a session.
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
  --dev-stub-sources
        Only in builds carrying the `test-encoders` feature (never a release
        build): capture from the synthetic sources instead of the display, the
        speakers and a microphone. **Nothing real is captured.** It exists so the
        whole pipeline — including the microphone path, which has no non-Windows
        backend — can be driven end to end on a development host.

configuration:
  %LOCALAPPDATA%\\localplay\\config.toml on Windows (the directory
  $LOCALAPPDATA points at may be redirected for a scratch run); the example
  file is `config.example.toml`. localplay never reads configuration from
  environment variables.
",
    );
    // The hotkey belongs to the runtime, not the flag list; naming the chord here would mean
    // reading the config for `--help`, which the flag parser does not do.
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
    /// `--mode <buffer|session>`: an override of `[recorder] mode` from the file. `None`
    /// means "whatever the file says".
    pub mode: Option<RecordingMode>,
    /// `--dev-stub-sources`: the synthetic capture/audio/microphone sources, for a host that is not
    /// the platform this application records on. Feature-gated like `--dev-software-encoder`: a
    /// build that captured nothing while looking like a recording would be worse than one that
    /// cannot be demonstrated.
    pub stub_sources: bool,
}

/// The flags the `buffer` subcommand accepted, before the config file is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferArgs {
    pub self_test_clip_after: Option<u64>,
    pub dev_software_encoder: bool,
    pub mode: Option<RecordingMode>,
    pub stub_sources: bool,
}

/// Parse the flags the `buffer` subcommand accepts (everything after `buffer`).
///
/// Strict on purpose: an unrecognised argument is an error, because this parser exists for a
/// verification flag that must provably have been understood.
pub fn parse_buffer_args(args: &[String]) -> Result<BufferArgs> {
    let mut parsed = BufferArgs {
        self_test_clip_after: None,
        dev_software_encoder: false,
        mode: None,
        stub_sources: false,
    };
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
                parsed.self_test_clip_after = Some(parse_self_test_seconds(&value)?);
            }
            "--mode" => {
                let value = value_of(&mut it, "--mode")?;
                parsed.mode = Some(value.parse::<RecordingMode>().context("--mode")?);
            }
            "--dev-software-encoder" => parsed.dev_software_encoder = true,
            "--dev-stub-sources" => parsed.stub_sources = true,
            other => match other.strip_prefix("--self-test-clip-after=") {
                Some(value) => parsed.self_test_clip_after = Some(parse_self_test_seconds(value)?),
                None => match other.strip_prefix("--mode=") {
                    Some(value) => {
                        parsed.mode = Some(value.parse::<RecordingMode>().context("--mode")?)
                    }
                    None => bail!("unexpected argument {other:?}\n\n{}", help()),
                },
            },
        }
    }
    Ok(parsed)
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
/// The requested hold (`--self-test-clip-after`) is a floor, not the whole rule: a clip spliced
/// before the pre-roll is complete would say nothing about the configured window, so the wait
/// is never shorter than one full pre-roll plus post-roll. Pure, so a unit test pins the rule.
pub fn self_test_need_ms(after_seconds: u64, pre_seconds: u64, post_seconds: u64) -> u64 {
    (after_seconds * 1000).max((pre_seconds + post_seconds) * 1000)
}

/// The `buffer` subcommand, as `main.rs` calls it: flags from the process's own arguments.
pub fn run_buffer() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let parsed = parse_buffer_args(&args)?;
    run_buffer_with(BufferOptions {
        app_dir: app_data_dir(),
        self_test_clip_after: parsed.self_test_clip_after,
        dev_software_encoder: parsed.dev_software_encoder,
        mode: parsed.mode,
        stub_sources: parsed.stub_sources,
    })
}

/// The `buffer` subcommand: capture, encode, segment ring, hotkey, clip.
///
/// Split from [`run_buffer`] so an integration test can drive the whole command in-process with
/// its own data directory; the self-test trigger must be reachable from a test.
pub fn run_buffer_with(opts: BufferOptions) -> Result<()> {
    let app_dir = opts.app_dir;
    let cfg_path = app_dir.join("config.toml");
    let cfg = if cfg_path.is_file() {
        Config::load(&cfg_path)?
    } else {
        Config::from_toml(include_str!("../../../config.example.toml"))?
    };
    let Config {
        recorder,
        buffer,
        encode,
        capture,
        audio,
        storage,
        mic,
        hotkeys,
        events,
        games,
        ..
    } = cfg;
    // `[audio] enabled = false` is carried to the engine rather than interpreted here: it asks for
    // a video-only recording, with no audio input declared, no loopback listener and no audio
    // stream. (Microphone on with game audio off, whose one-track container would be labelled
    // `Game Audio`, is refused in the recorder.)
    // Read before the move: these feed the CLI's own log line, written after the engine is
    // capturing. The rate printed comes from the engine rather than `encode.fps`, because the
    // engine may have measured that this machine cannot hold the configured rate and be pacing
    // lower (see `RecorderStatus::effective_fps`).
    let (pre_seconds, post_seconds) = (buffer.pre_seconds, buffer.post_seconds);
    let policy = Policy::new(&events, pre_seconds)?;
    // `--mode` overrides the file, and the resolved value is what the CLI logs and what the
    // engine is told: decided once here, never re-derived below.
    let mode = opts.mode.unwrap_or(recorder.mode);
    let encode_fps = encode.fps;

    let bin = FfmpegBinaries::discover(None)?;
    let hotkey = localplay_events::hotkey::Hotkey::parse(&hotkeys.clip)?;
    // The event sources need the application data directory too (GSI token, generated cfg file),
    // and the recorder takes it by value below.
    let data_dir = app_dir.clone();

    // `--dev-software-encoder` exists only with the test-encoders feature; it selects libx264,
    // needs no GPU vendor, and is never reachable from the config file. The engine applies the
    // gate (see `localplay_recorder::resolve_encoder`).
    let dev_software = opts.dev_software_encoder;
    let stub_sources = opts.stub_sources;

    let recorder = Recorder::start_with_options(
        RecorderConfig {
            bin,
            app_data_dir: app_dir,
            buffer,
            encode,
            storage,
            capture,
            // The `[audio]` section's only parser is the CLI, so it carries the values across: `enabled`
            // and `bitrate_kbps` are honoured (video-only recording, ffmpeg's `-b:a`); `codec` and
            // `source` are refused in `Config::validate`, each having one implemented value.
            audio_bitrate_kbps: Some(audio.bitrate_kbps),
            audio_enabled: Some(audio.enabled),
            // WGC + WASAPI on Windows, synthetic stubs elsewhere (`localplay_capture::platform` decides,
            // and on Windows refuses to fall back to a stub). `--dev-stub-sources` selects the synthetic
            // pair explicitly, microphone included (which has no non-Windows backend), with the same gate
            // as `--dev-software-encoder`.
            sources: choose_sources(stub_sources, encode_fps)?,
            dev_software_encoder: dev_software,
        },
        RecorderOptions {
            mode,
            mic,
            games,
            game: None,
        },
    )?;

    // From here the engine is capturing, or with `[games] auto_record` on, armed and waiting for a
    // game. This line is the CLI's own (the hotkey is what only it knows). The chord printed is the
    // parsed one, so `ctrl+f8` reads back as `Ctrl+F8` as in the desktop shell. The rate is the one
    // the pipeline runs at; when below the configured `encode.fps` the engine has already logged
    // why (see `log_rate_decision` in `localplay-recorder`).
    let status = recorder.status();
    if status.watching_games {
        tracing::info!(
            "armed in {} mode at up to {}fps: nothing is captured until a watched game \
             starts (games.auto_record = true), and each game is recorded as its own \
             session; press {} to clip during one",
            mode,
            status.configured_fps,
            hotkey
        );
    } else {
        tracing::info!(
            "{} {pre_seconds}s pre / {post_seconds}s post at {}fps; press {} to clip",
            if mode.is_full_session() {
                "recording the whole session,"
            } else {
                "buffering"
            },
            status.effective_fps,
            hotkey
        );
    }

    // The game-event sources. Started after the recorder (a source cannot ask for a clip before
    // there is anything to clip) and before the hotkey, so an early event is not lost to a wait
    // already running.
    //
    // Self-test mode does not start them: it takes exactly one clip, and a source asking for one
    // of its own would break that (and would bind a port or poll a game on a run meant to touch
    // nothing but its own capture).
    let sources = match opts.self_test_clip_after {
        Some(_) => {
            tracing::info!(
                "self-test mode: the game-event sources are not started (the run takes \
                 exactly one clip)"
            );
            let (_sink, events) = mpsc::channel();
            EventSources {
                events,
                _lol: None,
                _gsi: None,
                reports: Vec::new(),
            }
        }
        None => start_event_sources(&data_dir, &events),
    };

    // The hotkey listener is installed in both modes: it is part of the shipping startup path, and
    // self-test mode must not be a different program. Self-test just never reads it.
    //
    // `listen` fails loudly when the chord cannot be registered (held by another application, or
    // the desktop app on the same `config.toml`); a buffer run with a silently dead hotkey is not
    // worth starting.
    let hotkeys = hotkey::listen(hotkey).context("installing the clip hotkey")?;

    // The verification-only trigger (`--self-test-clip-after`): an in-process call to the same
    // media-time trigger the hotkey drives, never synthesised input.
    if let Some(after_seconds) = opts.self_test_clip_after {
        if recorder.status().watching_games {
            tracing::warn!(
                "self-test: games.auto_record is on, so the recording this run clips from \
                 does not exist yet — the self-test waits for a watched game to start"
            );
        }
        return self_test_clip(&recorder, after_seconds, pre_seconds, post_seconds);
    }

    // The driver loop. Every path out of it stops the recorder, which flushes the encoder, closes
    // the capture session and (in session mode) writes the session file.
    loop {
        if hotkey::wait_for_press(&hotkeys, HOTKEY_POLL) {
            if let Err(err) = recorder.clip_now() {
                // A recorder armed for game detection has nothing to clip until a game starts: a state, not a
                // failure.
                if recorder.is_armed() && !recorder.is_running() {
                    tracing::info!(
                        "nothing is being recorded yet ({err:#}); the hotkey takes a clip \
                         once a watched game starts"
                    );
                } else {
                    // The engine logs what went wrong; stopping first flushes the encoder and closes the capture
                    // session before the process gives up.
                    let _ = recorder.stop();
                    return Err(err.context("taking a clip"));
                }
            }
        } else {
            std::thread::sleep(HOTKEY_POLL);
        }

        // Anything the integrations derived, one at a time. A highlight is a clip through the call the
        // hotkey just made; a marker is recorded and takes no footage. `try_recv` rather than `recv`,
        // so a quiet game cannot make the hotkey wait.
        while let Ok(event) = sources.events.try_recv() {
            if let Err(err) = act_on_burst(&recorder, event, &sources.events, &policy) {
                // No recording exists to clip or mark; a game event arriving then is not a reason to stop
                // watching for a game.
                if recorder.is_armed() && !recorder.is_running() {
                    tracing::info!("the event is not recorded: {err:#}");
                    continue;
                }
                let _ = recorder.stop();
                return Err(err);
            }
        }

        if !recorder.is_running() && !recorder.is_armed() {
            // The engine stopped on its own (failed capture, scratch cap violation) and kept the reason in
            // its status; `stop()` reports it.
            recorder.stop()?;
            bail!("the recorder stopped; see the log above");
        }
    }
}

/// The verification-only self-test trigger: wait for a full window, take one clip, stop.
///
/// * the wait is on the ring's media timeline (`span_ms`), the clock the trigger uses, and is
///   never shorter than a full pre-roll plus post-roll ([`self_test_need_ms`]);
/// * the trigger is [`Recorder::clip_now`], the hotkey's own call, so the trigger line
///   (`hotkey pressed: media=... wall=... (drift ...ms)`) and the `wrote ...` line are exactly
///   the hotkey's;
/// * nothing here sends, simulates or injects a keyboard or mouse event, or enumerates windows.
///
/// Afterwards the recorder is stopped and the process exits with the clip's own outcome, which
/// makes the run scriptable.
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
    // Flush the encoder and close the capture session before exiting, so the exit code comes from
    // the engine's own shutdown and not from a drop().
    recorder.stop()
}

fn app_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("localplay")
}

/// The capture, audio and microphone sources this run uses.
///
/// `Sources::Platform` is the shipping choice: WGC + WASAPI on Windows, the synthetic pair
/// elsewhere (`localplay_capture::platform` refuses to substitute a stub on Windows).
///
/// `--dev-stub-sources` selects the synthetic sources explicitly, microphone included; the
/// microphone has no non-Windows backend, so this is the only way to drive that path on a
/// development host. Gated like `--dev-software-encoder`.
fn choose_sources(stub_sources: bool, fps: u32) -> Result<Sources> {
    if !stub_sources {
        return Ok(Sources::Platform);
    }
    #[cfg(feature = "test-encoders")]
    {
        tracing::warn!(
            "--dev-stub-sources: the synthetic capture, audio and microphone sources are in \
             use. NOTHING REAL IS CAPTURED — this is for pipeline smoke tests only, and the \
             recording it produces will show a test pattern, two synthetic tones and silence \
             from the display."
        );
        Ok(Sources::Stub(StubConfig {
            width: STUB_CAPTURE_SIZE.0,
            height: STUB_CAPTURE_SIZE.1,
            fps,
        }))
    }
    #[cfg(not(feature = "test-encoders"))]
    {
        let _ = fps;
        bail!(
            "--dev-stub-sources requires building with `--features test-encoders` (the same \
             gate --dev-software-encoder has): a build that captures nothing while looking \
             like a recording is not something to ship"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_buffer_flags_default_to_the_shipping_behaviour() {
        // No flags: the hotkey path is unchanged, the self-test is not armed, the mode comes from the
        // file and the real sources are used. The case a user runs.
        let parsed = parse_buffer_args(&[]).expect("no flags is a valid command line");
        assert_eq!(parsed.self_test_clip_after, None);
        assert!(!parsed.dev_software_encoder);
        assert_eq!(parsed.mode, None, "the file's [recorder] mode decides");
        assert!(
            !parsed.stub_sources,
            "the synthetic sources are never implicit"
        );
    }

    #[test]
    fn the_self_test_flag_parses_seconds_in_either_form() {
        assert_eq!(
            parse_buffer_args(&args(&["--self-test-clip-after", "45"]))
                .expect("spaced form")
                .self_test_clip_after,
            Some(45)
        );
        assert_eq!(
            parse_buffer_args(&args(&["--self-test-clip-after=45"]))
                .expect("= form")
                .self_test_clip_after,
            Some(45)
        );
        // The two forms are one setting: the last one wins, and an unrelated flag is unaffected.
        let parsed = parse_buffer_args(&args(&[
            "--self-test-clip-after=10",
            "--dev-software-encoder",
            "--self-test-clip-after",
            "20",
        ]))
        .expect("last one wins");
        assert_eq!(parsed.self_test_clip_after, Some(20));
        assert!(parsed.dev_software_encoder);
    }

    /// The mode is settable from the command line in either form, as an override: with no flag the
    /// file decides (`None`).
    #[test]
    fn the_mode_flag_parses_and_overrides_the_file() {
        assert_eq!(
            parse_buffer_args(&args(&["--mode", "session"]))
                .expect("the spaced form")
                .mode,
            Some(RecordingMode::FullSession)
        );
        assert_eq!(
            parse_buffer_args(&args(&["--mode=buffer"]))
                .expect("the = form")
                .mode,
            Some(RecordingMode::ReplayBuffer)
        );
        let err = parse_buffer_args(&args(&["--mode", "everything"]))
            .expect_err("an unknown mode must be refused")
            .to_string();
        assert!(
            err.contains("mode"),
            "the error names the flag and the setting: {err}"
        );
    }

    /// The stub-sources flag exists only in a build carrying the software-encoder feature, the same
    /// gate as `--dev-software-encoder`.
    #[test]
    fn the_stub_sources_flag_is_gated_behind_the_test_encoders_feature() {
        if cfg!(feature = "test-encoders") {
            let sources = choose_sources(true, 60).expect("with the feature it is selectable");
            assert!(matches!(sources, Sources::Stub(_)));
            assert!(matches!(
                choose_sources(false, 60).unwrap(),
                Sources::Platform
            ));
        } else {
            let err = choose_sources(true, 60)
                .expect_err("without the feature the flag must be refused")
                .to_string();
            assert!(
                err.contains("test-encoders"),
                "the error names the gate: {err}"
            );
        }
    }

    #[test]
    fn a_bad_self_test_value_is_refused_rather_than_defaulted() {
        // A verification flag that silently did nothing would be worse than a crash: the run would look
        // like it verified a trigger it never armed.
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
        assert!(
            err.contains("--clip-after"),
            "must name the bad argument: {err}"
        );
        assert!(err.contains("usage:"), "must show the usage: {err}");
    }

    #[test]
    fn the_help_documents_that_no_input_is_synthesised() {
        // The flag's whole justification; if this sentence is dropped, the reason for the flag is lost.
        let help = help();
        assert!(help.contains("--self-test-clip-after"));
        assert!(help.contains("no keyboard or mouse input"));
        assert!(help.contains("not enumerate windows"));
        // And the mode flags are documented where a user looks for them.
        assert!(help.contains("--mode <buffer|session>"));
        assert!(help.contains("--dev-stub-sources"));
        assert!(
            help.contains("Nothing real is captured"),
            "the stub flag says what it costs"
        );
    }

    #[test]
    fn the_self_test_wait_is_never_shorter_than_the_configured_window() {
        // Requested hold above the window: the request wins.
        assert_eq!(self_test_need_ms(45, 5, 3), 45_000);
        // Requested hold below it: a full pre-roll plus post-roll is the floor.
        assert_eq!(self_test_need_ms(2, 5, 3), 8_000);
        assert_eq!(self_test_need_ms(8, 5, 3), 8_000);
    }
}
