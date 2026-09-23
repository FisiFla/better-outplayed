//! The localplay CLI — the headless front-end over the recording engine.
//!
//! This crate is a *driver*, and nothing else. The pipeline (capture → audio → encode →
//! segment ring → clip) lives in `localplay-recorder`, which the desktop shell drives
//! too; what is left here is what only a headless front-end needs:
//!
//! 1. parse the config file (the CLI owns `[hotkeys]` and `[events]`; the recorder's
//!    `[buffer]`/`[encode]`/`[storage]` sections are shared types),
//! 2. start a [`Recorder`],
//! 3. install the global hotkey and call [`Recorder::clip_now`] on a press,
//! 4. stop the recorder on the way out.
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

use anyhow::{bail, Result};
use config::Config;
use localplay_events::hotkey;
use localplay_media::FfmpegBinaries;
use localplay_recorder::{Recorder, RecorderConfig, Sources};
use std::path::PathBuf;
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
    let Config { buffer, encode, storage, hotkeys, .. } = cfg;
    // Read before the move: this line is the CLI's own, and it is logged after the engine
    // is already capturing.
    let (pre_seconds, post_seconds, fps) =
        (buffer.pre_seconds, buffer.post_seconds, encode.fps);

    let bin = FfmpegBinaries::discover(None)?;
    let hotkey = localplay_events::hotkey::Hotkey::parse(&hotkeys.clip)?;

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

        if !recorder.is_running() {
            // The engine stopped on its own — a failed capture, a scratch cap violation —
            // and it kept the reason in its status. `stop()` reports it.
            recorder.stop()?;
            bail!("the recorder stopped; see the log above");
        }
    }
}

fn app_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("localplay")
}
