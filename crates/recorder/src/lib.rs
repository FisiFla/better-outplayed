//! The recording engine: capture → audio → encode → segment ring → clip.
//!
//! # Why this crate exists
//!
//! Recording used to be orchestration inside the CLI binary: the capture loop, the
//! post-roll wait, the trigger, the clip index write and the storage pass all lived in
//! `apps/localplay-cli/src/lib.rs`. That made the CLI the only thing in the project that
//! could record — and the CLI is a headless PoC front-end, not the application a user
//! runs. The desktop shell could browse, trim and delete clips but could not produce one.
//!
//! So the orchestration moved here, unchanged and behind a small API, and both front-ends
//! are drivers over it:
//!
//! * the CLI parses its config, starts a [`Recorder`], installs the global hotkey and
//!   calls [`Recorder::clip_now`] on a press;
//! * the desktop shell holds one in `AppState` and exposes start/stop/status/clip over
//!   IPC.
//!
//! # What it owns, and what it deliberately does not
//!
//! It owns the capture backend, the audio backend, the encoder, the [`RingBuffer`], the
//! [`FramePacer`], the capture clock, the background thread that pumps them, and the clip
//! index insertion. It owns **no** hotkey (a driver's business — the CLI installs one, the
//! GUI calls [`Recorder::clip_now`] from a button) and **no** UI, and it reads no
//! configuration file: the front-ends hand it a [`RecorderConfig`].
//!
//! # Why a clip was taken
//!
//! A trigger carries a [`ClipReason`]. A manual clip records nothing but the clip; a clip an
//! integration asked for writes the reason into the `events` table (spec §5.5) linked to the
//! clip it produced, so the session timeline says *why* the footage exists. Both paths are
//! the same trigger — [`Recorder::clip_now_with`] is [`Recorder::clip_now`] with a reason —
//! which is what keeps an event-triggered clip subject to the same pre-roll, post-roll and
//! media-time semantics as a hotkey press.
//!
//! An event that is a *marker* rather than a highlight ([`EventKind::is_highlight`]) is
//! recorded without a clip, through [`Recorder::note_event`]. No second trigger path is
//! involved in either case.
//!
//! # The thread
//!
//! [`Recorder::start`] does the whole startup handshake on the calling thread — open the
//! index, apply the storage policy, resolve and smoke-test the encoder, create the capture
//! backend, build the ring, spawn ffmpeg — and returns its errors *there*, so a machine
//! that cannot encode fails at startup rather than inside a thread nobody is watching.
//! Only the pump loop runs on the background thread it spawns.
//!
//! [`Recorder::clip_now`] hands the trigger to that thread, because the trigger has to
//! pump the encoder — it is the same capture/encoder/ring the loop owns, and it may wait
//! [`POST_ROLL_MARGIN`] plus the post-roll for the footage to be written. Everything else
//! ([`Recorder::status`]) reads atomics and never blocks the loop.
//! # The two modes, the microphone and the game watcher
//!
//! Phase 5 added three things to the engine, and each is opt-in or additive:
//!
//! * [`RecordingMode`] (spec §6, `[recorder] mode`). [`RecordingMode::ReplayBuffer`] is the
//!   ring this crate has always run and remains the default; [`RecordingMode::FullSession`]
//!   writes the session into a per-session directory that the scratch cap does **not**
//!   apply to, and concatenates it into one `session-<timestamp>.mp4` at stop (see
//!   [`session`]). Both modes open a `sessions` row and are managed by the retention pass.
//! * The microphone ([`MicSection`], `[mic] enabled`, off by default). When it is on, the
//!   encoder is configured with a second audio input and the microphone's blocks are fed to
//!   it, so a clip — and a session file — carries two audio tracks. When it is on and the
//!   backend cannot start, the recording fails at start rather than producing video with a
//!   silent voice track.
//! * The game watcher ([`localplay_events::process::GamesSection`], `[games] auto_record`,
//!   off by default). With it on, [`Recorder::start`] starts **nothing** game-related
//!   watching-wise except the watcher itself: a recording is started when a watched game
//!   starts and stopped when it stops. With it off (the default) no watcher thread, no
//!   process enumeration and no request exist at all — it is not a flag on a watcher that
//!   runs anyway.
//!
//! # The one rate every path shares
//!
//! [`Recorder::start`] is [`Recorder::start_with_options`] with [`RecorderOptions::default`]
//! (replay buffer, no microphone, nothing watched), which is what both front-ends' existing
//! code means; the options struct carries the Phase 5 settings. The configuration struct
//! [`RecorderConfig`] itself was not widened, for a reason worth stating: the desktop shell
//! is a separate workspace that constructs `RecorderConfig` field by field, so a new
//! required field there would break a crate this work may not touch. The settings that are
//! not part of the recorder's own sections travel in [`RecorderOptions`] instead.
//!
//! [`Recorder::clip_now`] hands the trigger to that thread, because the trigger has to
//! pump the encoder — it is the same capture/encoder/ring the loop owns, and it may wait
//! [`POST_ROLL_MARGIN`] plus the post-roll for the footage to be written. Everything else
//! ([`Recorder::status`]) reads atomics and never blocks the loop.
//!
//! # Properties that must not regress
//!
//! Each of these was paid for once, and each has a test:
//!
//! * The trigger is expressed in the **ledger's media time** (`span_ms`), never the wall
//!   clock, and the post-roll budget is `post_ms + margin`. Media time is the footage the ring
//!   can prove is on disk, which is the only clock a splice can be cut on — a wall-clock
//!   trigger asked for footage the encoder might not have written yet.
//! * The pacer decides **before** the frame is materialised, and non-due frames are
//!   drained with [`CaptureBackend::discard_pending`] so the GPU readback is skipped.
//! * The encoder is smoke-tested **before** any capture backend is created, so an
//!   unusable encoder never opens a capture session on the user's display. The microphone is
//!   built and started on the same side of that line (see `Prepared::begin`).
//! * Clips are spliced losslessly (`-c copy`) and indexed with the row written before the
//!   file could ever be evicted (spec §8.2).
//! * The storage cleanup pass runs at startup and every [`CLEANUP_INTERVAL`], over clips
//!   **and** sessions independently ([`cleanup_pass`]).
//! * The status line (`frames= segments= bytes= span= dropped= dropped_audio= dropped_mic=
//!   skipped= fps=`) stays observable, and `running`/counters stay readable from another
//!   thread.
//! * One writer thread per input: the pump submits video, game audio and microphone from its
//!   own thread into the encoder's per-input queues, never writing two inputs synchronously
//!   ([`pump_once_counted_with_mic`]).

pub mod config;
pub mod fps;
pub mod index;
pub mod pump;
pub mod session;

#[cfg(test)]
mod tests;

pub use config::{
    BufferSection, EncodeSection, MicSection, RecorderSection, RecordingMode, SessionStorageRules,
    StorageSection,
};
pub use fps::FpsDecision;
pub use index::{
    cleanup_pass, index_clip, index_event, now_ms, open_clip_index, unix_seconds, CleanupReport,
};
pub use pump::{
    guard_frame_size, pump_once, pump_once_counted, pump_once_counted_with_mic, pump_until_span,
    pump_until_span_on, FramePacer, MediaRing, PumpCounts, RateMeter, FRAME_POLL,
    PACER_RESYNC_AFTER_INTERVALS, POST_ROLL_MARGIN, POST_ROLL_SCAN_INTERVAL, RATE_WINDOW,
};

use anyhow::{bail, Context, Result};
use localplay_capture::platform::{default_audio_backend, default_video_backend};
use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::wasapi_mic::{StubMicrophone, MICROPHONE_FORMAT};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_encoder::probe::select_vendor;
use localplay_encoder::{
    EncodeConfig, Encoder, FfmpegEncoder, MicAudioSpec, ThroughputMeasurement, Vendor, VideoCodec,
};
use localplay_events::process::{GamesSection, PresenceChange, WatchHandle};
use localplay_events::{CaptureClock, GameEvent};
use localplay_media::FfmpegBinaries;
use localplay_replay::buffer::{BufferConfig, BufferStats, RingBuffer};
use localplay_replay::splice::ClipMetadata;
use localplay_store::Store;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The frame size the synthetic capture backend produces off Windows.
///
/// Real Windows capture (WGC) reports the primary monitor's native resolution; there
/// is no monitor to query on macOS, so the stub stands in at a fixed, representative
/// size. It is deliberately not the old 1920x1080 encoder default, so a macOS run
/// makes it plain that the rawvideo pipe is sized from the backend, not a constant.
pub const STUB_CAPTURE_SIZE: (u32, u32) = (1280, 720);

/// How often the scratch ring is scanned, the status published, and the status line
/// emitted while the buffer runs.
///
/// One tick for all three: the scan is what makes new segments visible, and the counters
/// that are published and logged (`segments=`, `bytes=`, `span=`) only move when a
/// segment is finalised, so scanning and reporting at the same instant costs nothing
/// extra and keeps the log line's numbers exactly the ones the ring reported.
pub const STATUS_INTERVAL: Duration = Duration::from_millis(200);

/// At most one "the encoder cannot sustain the configured fps" warning per this long.
///
/// The condition it reports (`dropped=` rising) is evaluated every [`STATUS_INTERVAL`], and
/// a machine that genuinely cannot encode at the configured rate stays in that state for
/// the whole run: warning once per log line would bury everything else in the log. The
/// `debug` status line remains the continuous record; this is the part that must not be
/// missable.
pub const DROP_WARN_INTERVAL: Duration = Duration::from_secs(10);

/// How often the storage policy is re-applied once the buffer is running (spec §8.1).
///
/// This is the **clips** directory's rule, not the scratch ring's: the ring evicts its
/// own segments to `buffer.scratch_cap_bytes` on every scan, while a clip is written once
/// and — before this — kept forever. The clips directory therefore only grows when a clip
/// is triggered, and one pass costs a `list_clips` plus a `SUM`, i.e. a few hundred
/// microseconds on a library of thousands of rows. Five minutes is long enough that the
/// cost is invisible in a capture path that submits 30-60 frames a second, and short
/// enough that a session's clips cannot drift far past `storage.max_total_bytes` before
/// they are managed. A fresh run applies the policy at startup, so anything a previous
/// session left over is handled immediately rather than five minutes in.
pub const CLEANUP_INTERVAL: Duration = Duration::from_secs(300);

/// Where the frames come from.
///
/// [`Sources::Platform`] is the shipping path and the only one that ever touches a
/// display: on Windows it is Windows Graphics Capture and WASAPI loopback, and elsewhere
/// it is the synthetic stubs (see `localplay_capture::platform`, which refuses to fall
/// back to a stub on Windows so a broken capture never looks like a working one).
///
/// [`Sources::Stub`] selects the synthetic sources **explicitly**. It exists so this
/// crate's tests can drive the whole engine — pacer, encoder, ring, trigger, index —
/// without touching the host's screen or its audio, and it is never reachable from a
/// configuration file.
#[derive(Debug, Clone, Copy)]
pub enum Sources {
    Platform,
    Stub(StubConfig),
}

/// Everything [`Recorder::start`] needs, resolved by the front-end.
///
/// The three sections are `config.toml`'s, verbatim ([`config`]), and the paths are the
/// application data directory plus whatever the sections overrode — the recorder applies
/// the "empty means the default" rule itself, so both front-ends resolve `scratch/`,
/// `clips/` and `localplay.db` the same way.
#[derive(Debug, Clone)]
pub struct RecorderConfig {
    /// Where ffmpeg and ffprobe live: discovered by the front-end (spec §5.4), so a
    /// missing sidecar is reported by the thing that knows where to look for it.
    pub bin: FfmpegBinaries,
    /// The application data directory. `scratch/`, `clips/` and `localplay.db` live here
    /// unless `[buffer].scratch_dir` / `[storage].clips_dir` name somewhere else.
    pub app_data_dir: PathBuf,
    pub buffer: BufferSection,
    pub encode: EncodeSection,
    pub storage: StorageSection,
    pub sources: Sources,
    /// Encode with libx264 instead of a hardware vendor. This is the CLI's
    /// `--dev-software-encoder`: it exists for pipeline smoke tests on a host with no GPU
    /// encoder, needs no vendor at all, and is only reachable from a build with the
    /// `test-encoders` feature — never from a configuration file (spec §3.2).
    pub dev_software_encoder: bool,
}

impl RecorderConfig {
    /// The scratch directory this configuration records into.
    pub fn scratch_dir(&self) -> PathBuf {
        resolve_dir(&self.app_data_dir, "scratch", &self.buffer.scratch_dir)
    }

    /// The clips directory this configuration writes into — the same rule the desktop
    /// shell's `AppPaths` applies, which is what makes the two operate on one library.
    pub fn clips_dir(&self) -> PathBuf {
        resolve_dir(&self.app_data_dir, "clips", &self.storage.clips_dir)
    }

    /// The sessions area: where a full session's segment directory and its concatenated
    /// file live ([`RecordingMode::FullSession`], `session`).
    ///
    /// Its own directory rather than a corner of `scratch/`, because the two are managed by
    /// different rules: the ring evicts its scratch to `buffer.scratch_cap_bytes` on every
    /// scan, while a session directory is only ever removed by its own finalise (or by the
    /// retention rules through its session row).
    pub fn sessions_dir(&self) -> PathBuf {
        resolve_dir(&self.app_data_dir, "sessions", &self.storage.sessions.sessions_dir)
    }

    /// The clip index both this engine and the desktop shell open.
    pub fn db_path(&self) -> PathBuf {
        self.app_data_dir.join("localplay.db")
    }
}

/// An empty setting means "under the application data directory" (spec §10).
fn resolve_dir(app_data_dir: &Path, default_name: &str, setting: &str) -> PathBuf {
    if setting.is_empty() {
        app_data_dir.join(default_name)
    } else {
        PathBuf::from(setting)
    }
}

/// Everything [`Recorder::start_with_options`] needs that is not in the configuration file's
/// recording sections.
///
/// [`Recorder::start`] is this struct's [`Default`]: replay buffer, no microphone, nothing
/// watched — the behaviour every caller had before Phase 5. It exists as its own struct
/// rather than as more fields on [`RecorderConfig`] because the desktop shell is a separate
/// workspace that builds a `RecorderConfig` field by field, and a new required field there
/// would break a crate this work does not touch. A front-end that wants a setting here
/// passes it; one that does not keeps compiling and keeps behaving exactly as it did.
#[derive(Debug, Clone, Default)]
pub struct RecorderOptions {
    /// What to record: the rolling buffer, or the whole session (see [`RecordingMode`]).
    pub mode: RecordingMode,
    /// The optional microphone track ([`MicSection`]; off by default).
    pub mic: MicSection,
    /// `[games]` — whether a watched game starting is itself a reason to record, and which
    /// titles count ([`GamesSection`]). `auto_record` defaults to `false`, and with it off
    /// nothing game-related is started at all.
    pub games: GamesSection,
    /// The game a driver detected, written into the `sessions` row this recording opens.
    /// `None` for a recording a person started (a hotkey, a button, the CLI's run).
    pub game: Option<String>,
}

// `Default` is derived, and it is the pre-Phase-5 behaviour by construction: `RecordingMode`
// defaults to the replay buffer, `MicSection` to off, `GamesSection` to `auto_record = false`
// and `game` to `None` — every field's own default. `Recorder::start` passes it, so a caller
// that has never heard of Phase 5 gets exactly what it always got.

/// What the recording engine is doing right now.
///
/// Read from another thread while the loop runs: every field is an atomic load (plus, for
/// `error`, a short-lived mutex), so a status read never waits for the capture loop and
/// the capture loop never waits for a reader.
#[derive(Debug, Clone, PartialEq)]
pub struct RecorderStatus {
    /// `false` once `stop()` has returned, and once the loop has failed. A front-end that
    /// is polling this can tell "recording" from "stopped" without owning the thread.
    pub running: bool,
    /// Video frames submitted to the encoder (the status line's `frames=`).
    pub frames: u64,
    /// Completed segments in the ring.
    pub segments: u64,
    /// Bytes the ring holds on disk.
    pub bytes: u64,
    /// Media time the ring can prove is on disk, in ms — the position the trigger is
    /// taken from.
    pub span_ms: u64,
    /// Encoder video frames dropped because its queue was full.
    pub dropped: u64,
    /// The same for audio blocks.
    pub dropped_audio: u64,
    /// The same for microphone blocks. Zero when this recording has no microphone track,
    /// and zero — not absent — when it has one and nothing was dropped.
    pub dropped_mic_audio: u64,
    /// Whether this recording carries a microphone track at all (`[mic] enabled`).
    pub mic: bool,
    /// The loopback port the encoder's microphone input is declared on, when there is one.
    /// `None` means the second input does not exist, which is what the encoder's own
    /// `mic_port()` answers — reported so a front-end can say *which* input is in use
    /// rather than believing the configuration.
    pub mic_port: Option<u16>,
    /// The mode this recorder was started in ([`RecordingMode`]).
    pub mode: RecordingMode,
    /// The game being recorded, when a watched game triggered this recording
    /// (`[games] auto_record = true`); `None` while nothing is being recorded, and for a
    /// recording a person started.
    pub game: Option<String>,
    /// Whether a game watcher is armed for this recorder. `false` unless `[games]
    /// auto_record` was on at start — with it off there is no watcher at all, which is what
    /// this reports. While it is `true` and `running` is `false`, the recorder is **armed
    /// and waiting for a game**: a front-end's "is it recording?" question is `running`,
    /// and "is it going to?" is this.
    pub watching_games: bool,
    /// Frames the backend offered that the pacer skipped *without* reading them back.
    pub skipped: u64,
    /// The achieved frame rate over the last [`RATE_WINDOW`]; 0.0 before one has closed.
    pub fps: f64,
    /// What `encode.fps` asked for, so the two can be shown side by side.
    pub configured_fps: u32,
    /// The rate the pipeline is actually running at — the pacer's rate and the encoder
    /// child's `-framerate`, one number. Below `configured_fps` when the startup probe
    /// measured that this machine cannot hold the configured rate and adaptation is on
    /// (startup logs that in as many words). It is the rate an achieved-rate readout should
    /// be compared against; the media timeline does not depend on it (frames carry their
    /// arrival timestamps).
    pub effective_fps: u32,
    /// Wall clock minus media time, in ms. Positive means the footage the ring can prove it
    /// has is behind the wall clock — the ring can only count *finished* segments, so this
    /// includes the encoder's lag as well as any clock divergence. Expected to stay near its
    /// start-up offset (ffmpeg's start-up plus one segment) on a healthy machine; the
    /// trigger's media-time arithmetic is anchored to `span_ms`, so it is worth watching.
    pub drift_ms: i64,
    /// Clips this session has written.
    pub clips: u64,
    /// Set when the loop stopped because of a failure, so a front-end can say why.
    pub error: Option<String>,
}

impl RecorderStatus {
    /// The status of an engine that is not running, for a front-end that has never
    /// started one. `configured_fps` is 0 because nothing has been configured.
    pub fn stopped() -> Self {
        Self {
            running: false,
            frames: 0,
            segments: 0,
            bytes: 0,
            span_ms: 0,
            dropped: 0,
            dropped_audio: 0,
            dropped_mic_audio: 0,
            mic: false,
            mic_port: None,
            mode: RecordingMode::default(),
            game: None,
            watching_games: false,
            skipped: 0,
            fps: 0.0,
            configured_fps: 0,
            effective_fps: 0,
            drift_ms: 0,
            clips: 0,
            error: None,
        }
    }
}

/// Why a clip was taken.
///
/// This is the difference between "a clip exists" and "the session timeline knows why",
/// which is what spec §5.5's `events` table is for. It is also the seam the two game
/// integrations come in through (spec §7.1, §7.2): a driver hands the engine a derived
/// [`GameEvent`], and the engine takes the clip through exactly the path a hotkey press
/// takes — one trigger, one window, one splice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipReason {
    /// A hotkey press or a button. No `events` row is written: there is no game event to
    /// record, and a synthetic one would put a marker on the session timeline at which
    /// nothing happened.
    Manual,
    /// A game event derived by an integration. The reason is written to `events` and linked
    /// to the clip it produced (or to nothing, if the clip could not be indexed).
    GameEvent(GameEvent),
}

impl ClipReason {
    /// How the trigger is described in the log — and, for an event, in the `events` row.
    pub fn describe(&self) -> String {
        match self {
            ClipReason::Manual => "hotkey pressed".to_string(),
            ClipReason::GameEvent(event) => format!("{} event {}", event.source, event.kind),
        }
    }

    /// The event behind this trigger, when there is one.
    pub fn event(&self) -> Option<&GameEvent> {
        match self {
            ClipReason::Manual => None,
            ClipReason::GameEvent(event) => Some(event),
        }
    }
}

impl From<GameEvent> for ClipReason {
    fn from(event: GameEvent) -> Self {
        ClipReason::GameEvent(event)
    }
}

/// What a trigger produced.
#[derive(Debug, Clone)]
pub struct RecordedClip {
    /// The file that was written, with what the splice measured it at.
    pub metadata: ClipMetadata,
    /// The `clips` row, when the index write succeeded. `None` means the file is on disk
    /// and unindexed — the case [`index_clip`] logs at ERROR and never hides.
    pub id: Option<i64>,
    /// The clip's first frame on the ledger's media timeline, which is what the index
    /// row's `started_at` carries.
    pub started_at_ms: u64,
}

/// A running recording session.
///
/// [`Recorder::start`] returns one with the pump loop already running on its own thread;
/// dropping it stops that thread (see [`Recorder::stop`]).
pub struct Recorder {
    /// Commands to the recording that is running **now**, if one is.
    ///
    /// A game-driven recorder replaces this on every start and clears it on every stop
    /// (see [`RecorderOptions::games`]), so [`Recorder::clip_now`] always reaches the
    /// recording that exists — and answers with a clear error when none does, which is a
    /// different state from "stopped": armed and waiting for a game is a state, not a
    /// recording. Dropping the last sender disconnects the loop, which is how a dropped
    /// `Recorder` stops its capture even if `stop()` is never called.
    commands: Arc<Mutex<Option<Sender<Command>>>>,
    /// The engine thread of a recording this `Recorder` started itself. Games mode leaves
    /// this `None`: the supervisor owns the thread, because it is the thing that starts and
    /// stops recordings ([`Recorder::stop`] signals the supervisor instead).
    thread: Mutex<Option<JoinHandle<Store>>>,
    /// The game-presence supervisor, when a front-end asked for one. `None` otherwise — and
    /// that `None` is the structural half of "with `auto_record = false` nothing is watched":
    /// there is no receiver, no handle and no thread to gate.
    supervisor: Mutex<Option<Supervisor>>,
    status: Arc<SharedStatus>,
    /// The mode this recorder was started in, reported by [`Recorder::status`].
    mode: RecordingMode,
    /// Whether this recorder was started with a microphone track ([`MicSection::enabled`]).
    mic: bool,
}

/// A request from a caller on another thread to the pump loop.
enum Command {
    /// Take a clip now, for this reason. The reply is the clip, or why there is none.
    Clip { reason: ClipReason, reply: Sender<Result<RecordedClip>> },
    /// Record an event that did not ask for a clip (a game or round boundary). The reply is
    /// the `events` row's id, so a caller can tell that it landed.
    Note { event: GameEvent, reply: Sender<Result<i64>> },
    /// Flush the encoder and end the loop.
    Stop,
}

/// The counters the loop publishes for [`Recorder::status`].
///
/// Every field is independent of every other, and a reader never uses one to decide
/// whether another is meaningful, so `Relaxed` is the honest ordering: no reader may
/// block the capture loop, and none of these values gates a memory access.
#[derive(Debug)]
struct SharedStatus {
    running: AtomicBool,
    frames: AtomicU64,
    segments: AtomicU64,
    bytes: AtomicU64,
    span_ms: AtomicU64,
    dropped: AtomicU64,
    dropped_audio: AtomicU64,
    skipped: AtomicU64,
    clips: AtomicU64,
    drift_ms: AtomicI64,
    /// The achieved rate, as `f64::to_bits` (there is no `AtomicF64`).
    fps: AtomicU64,
    /// The rate the encoder was told and the rate the configuration asked for, published per
    /// recording — so a recorder that starts its recordings later (a game watcher) reports
    /// the numbers of the recording that is running, not of the one before it.
    configured_fps: AtomicU64,
    effective_fps: AtomicU64,
    /// Microphone blocks the encoder's queue dropped (`Encoder::dropped_mic_audio_blocks`).
    dropped_mic: AtomicU64,
    /// The encoder's microphone loopback port; 0 means "no microphone input". A port is
    /// never 0, so the sentinel cannot collide with a real one.
    mic_port: AtomicU64,
    /// Whether a game watcher is armed (set while the supervisor thread lives).
    watching: AtomicBool,
    /// The game being recorded, if a watcher started this recording.
    game: Mutex<Option<String>>,
    /// The failure the loop stopped for, if any. A mutex rather than an atomic because it
    /// is a string and is written at most once per run — never on the hot path.
    error: Mutex<Option<String>>,
}

impl SharedStatus {
    fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            segments: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            span_ms: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            dropped_audio: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            clips: AtomicU64::new(0),
            drift_ms: AtomicI64::new(0),
            fps: AtomicU64::new(0.0f64.to_bits()),
            configured_fps: AtomicU64::new(0),
            effective_fps: AtomicU64::new(0),
            dropped_mic: AtomicU64::new(0),
            mic_port: AtomicU64::new(0),
            watching: AtomicBool::new(false),
            game: Mutex::new(None),
            error: Mutex::new(None),
        }
    }

    fn publish_rates(
        &self,
        frames: u64,
        skipped: u64,
        dropped: u64,
        dropped_audio: u64,
        fps: f64,
        drift_ms: i64,
    ) {
        self.frames.store(frames, Ordering::Relaxed);
        self.skipped.store(skipped, Ordering::Relaxed);
        self.dropped.store(dropped, Ordering::Relaxed);
        self.dropped_audio.store(dropped_audio, Ordering::Relaxed);
        self.fps.store(fps.to_bits(), Ordering::Relaxed);
        self.drift_ms.store(drift_ms, Ordering::Relaxed);
    }

    /// The microphone input's own drop counter, kept apart from the other two: it is read
    /// from a different encoder method and is only meaningful when this recording has a
    /// microphone at all (see [`RecorderStatus::mic`]).
    fn publish_mic(&self, dropped_mic: u64) {
        self.dropped_mic.store(dropped_mic, Ordering::Relaxed);
    }

    fn publish_ring(&self, segments: u64, bytes: u64, span_ms: u64) {
        self.segments.store(segments, Ordering::Relaxed);
        self.bytes.store(bytes, Ordering::Relaxed);
        self.span_ms.store(span_ms, Ordering::Relaxed);
    }

    /// Publish the per-recording numbers: the two rates, and the microphone's port (so a
    /// front-end can tell a recording with a microphone input from one without).
    fn publish_recording(&self, configured_fps: u32, effective_fps: u32, mic_port: Option<u16>) {
        self.configured_fps.store(u64::from(configured_fps), Ordering::Relaxed);
        self.effective_fps.store(u64::from(effective_fps), Ordering::Relaxed);
        self.mic_port.store(u64::from(mic_port.unwrap_or(0)), Ordering::Relaxed);
        self.dropped_mic.store(0, Ordering::Relaxed);
    }

    fn set_game(&self, game: Option<String>) {
        *self.game.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = game;
    }

    fn game(&self) -> Option<String> {
        self.game.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
}

impl Recorder {
    /// Start recording, with the behaviour every caller had before Phase 5: a replay buffer,
    /// no microphone, nothing watched. Exactly
    /// [`Recorder::start_with_options`] with [`RecorderOptions::default`].
    ///
    /// The whole startup handshake runs on this thread, in this order, because the order
    /// is the point:
    ///
    /// 1. open (and migrate) the clip index — the one step that fails hard;
    /// 2. apply the storage policy once, so a previous session's leftovers are managed now,
    ///    and recover whatever a crash left behind (`session::recover_sessions`);
    /// 3. resolve the encoder's vendor with a one-frame smoke test, **before** any capture
    ///    backend exists, so an unusable encoder never opens a session on the display;
    /// 4. create the capture backend and read its native size — and, when a microphone was
    ///    asked for, create and start it **before** that, so a microphone that cannot start
    ///    fails the recording instead of leaving it with a voice track nothing feeds;
    /// 5. build the encoder configuration from that size, **measure what the machine can
    ///    sustain at that size** and decide the rate the pipeline runs at
    ///    ([`FpsDecision`]), start the ring (or the session's own segment store), adopt what
    ///    is already on disk, decide the encoder's first segment number, spawn ffmpeg, and
    ///    open the `sessions` row this recording will be closed through;
    /// 6. start capture and audio;
    /// 7. spawn the pump loop.
    ///
    /// Nothing is captured before step 6, so the segment numbering decided in step 5
    /// cannot miss footage — and the throughput probe in step 5 runs before the capture
    /// session exists, so a probe failure costs a startup error rather than a session on
    /// the user's display.
    pub fn start(cfg: RecorderConfig) -> Result<Recorder> {
        Self::start_with_options(cfg, RecorderOptions::default())
    }

    /// [`Recorder::start`], with the Phase 5 settings: the recording mode, the microphone,
    /// and whether a watched game starting is a reason to record.
    ///
    /// With [`RecorderOptions::games`]' `auto_record` off (the default, and what
    /// [`Recorder::start`] passes) this begins recording immediately, exactly as it always
    /// has. With it on, this records **nothing yet**: it resolves everything that can be
    /// resolved without touching the display (steps 1–3 above), starts the game watcher, and
    /// returns a recorder that is *armed* — [`Recorder::status`]`().running` is `false` and
    /// `watching_games` is `true` — until a watched game starts. The rest of the handshake
    /// happens at that moment, because on Windows creating the video backend opens a Windows
    /// Graphics Capture session on the user's primary monitor: starting a recording is not
    /// something to do to somebody who is not playing.
    ///
    /// In that mode each game session is recorded on its own: a `sessions` row is opened when
    /// the game starts and closed when it stops (a full session is also concatenated then),
    /// and the retention pass runs between game sessions, on the supervisor's thread.
    pub fn start_with_options(cfg: RecorderConfig, opts: RecorderOptions) -> Result<Recorder> {
        // The shipping measurement: the selected encoder, at the capture's own size, for a
        // bounded budget. The binaries are cloned into the closure so the probe does not
        // borrow `cfg` while it is being moved into `start_inner`.
        let bin = cfg.bin.clone();
        let measure = move |encode_cfg: &EncodeConfig| {
            localplay_encoder::throughput::measure_sustainable_fps(
                &bin,
                encode_cfg,
                localplay_encoder::throughput::PROBE_BUDGET,
            )
        };
        if !opts.games.watching() {
            // Nothing is watched, so nothing here creates a channel, a receiver or a thread:
            // `auto_record = false` is structural, not a flag a watcher checks.
            return Self::start_inner(cfg, opts, &measure, None);
        }
        // `GamesSection::start` is the events crate's opt-in: it starts a watcher when
        // `auto_record` is on and returns `Ok(None)` — starting nothing — when it is off.
        // This branch only runs when `watching()` already said yes, and the `Option` is
        // handled rather than unwrapped so the two can never disagree silently.
        let (sink, changes) = mpsc::channel();
        let watch = opts.games.start(sink)?;
        Self::start_inner(cfg, opts, &measure, Some((changes, watch)))
    }

    /// [`Recorder::start`], with the throughput measurement injected.
    ///
    /// A test hook, and `cfg(test)` because of it: production reaches the same handshake
    /// through [`Recorder::start_with_options`], which passes the shipping probe.
    ///
    /// Split out so the *decision* — and, with it, the pairing of the pacer's rate with the
    /// encoder child's — can be tested without a machine whose encoder is slow: a test hands
    /// in a measurement of its own and asserts what the pipeline did with it. Production
    /// passes [`localplay_encoder::throughput::measure_sustainable_fps`].
    ///
    /// # The one rate
    ///
    /// [`FpsDecision::effective`] is written into `encode_cfg.fps` **once**, and both
    /// consumers read that field:
    ///
    /// * [`spawn_encoder`] hands the whole configuration to
    ///   [`localplay_encoder::FfmpegEncoder::spawn`], whose `-framerate` comes from
    ///   `EncodeConfig::fps` (`localplay_encoder::video_input_args` — the same function the
    ///   probe itself used, so the number measured and the number recorded are one argument);
    /// * the [`FramePacer`] is built from `encode_cfg.fps` — the encoder's own field, not
    ///   `cfg.encode.fps` and not a second computation of the minimum.
    ///
    /// That is deliberate and it is the whole of the fix: the pacer admits what the encoder
    /// was told, so the pipeline cannot declare a rate it does not deliver. If they are ever
    /// computed separately they can disagree — that is how issues #1 and #2 happened — so
    /// there is one binding, and `tests::the_pacer_and_the_encoder_are_told_the_same_rate`
    /// fails if a second one appears.
    ///
    /// The injected measurement applies to the recording this call starts. A game-driven
    /// recorder measures with the shipping probe instead, because its recording begins later,
    /// on the supervisor's thread, where a caller's closure is not there to borrow.
    #[cfg(test)]
    fn start_with_measure(
        cfg: RecorderConfig,
        measure: &dyn Fn(&EncodeConfig) -> Result<ThroughputMeasurement>,
    ) -> Result<Recorder> {
        Self::start_inner(cfg, RecorderOptions::default(), measure, None)
    }

    /// [`Recorder::start_with_options`]'s body, with the presence channel injected.
    ///
    /// `presence` is `Some` exactly when a game watcher is to drive this recorder: the
    /// changes the watcher reports (production: `GamesSection::start`; a test: a channel it
    /// owns) plus the watcher's handle, which is kept alive for as long as the supervisor
    /// runs. `None` means "record now".
    fn start_inner(
        cfg: RecorderConfig,
        opts: RecorderOptions,
        measure: &dyn Fn(&EncodeConfig) -> Result<ThroughputMeasurement>,
        presence: Option<(Receiver<PresenceChange>, Option<WatchHandle>)>,
    ) -> Result<Recorder> {
        // The sections this run reads, resolved once: the mode does not change them, and the
        // session directory is derived from the sessions area (see `Prepared::begin`).
        let scratch_dir = cfg.scratch_dir();
        let clips_dir = cfg.clips_dir();
        let sessions_dir = cfg.sessions_dir();

        // 1. The index — the one step that fails hard (see `open_clip_index`).
        let store = open_clip_index(&cfg.db_path())?;

        // 2. The storage policy, once, so a previous run's leftovers are managed now — over
        //    clips and sessions both, with the session rules the config carries.
        let mut cleanup = CleanupReport::default();
        cleanup_pass(&store, &cfg.storage, &mut cleanup);

        // 2b. What a crash left behind, before this run creates anything that recovery
        //     could confuse for its own: an unfinalised session is finished (or reported and
        //     retried next time), and a stray row is closed.
        let recovery = session::recover_sessions(
            &store,
            &sessions_dir,
            &cfg.bin,
            cfg.buffer.segment_time * 1000,
            &cfg.encode.codec,
        );
        log_recovery(&recovery);

        // 3. The encoder is resolved — and smoke-tested — before any capture backend exists
        //    (see `resolve_encoder` for why that order is load-bearing). In games mode this
        //    is the whole of the eager check: a machine that cannot encode says so now rather
        //    than when the first game starts.
        let (codec, vendor) = resolve_encoder(&cfg.bin, &cfg.encode, cfg.dev_software_encoder)?;

        let status = Arc::new(SharedStatus::new());
        let mode = opts.mode;
        let mic = opts.mic.enabled;
        let game = opts.game.clone();
        let prepared = Prepared {
            cfg,
            opts,
            codec,
            vendor,
            store: Some(store),
            scratch_dir,
            clips_dir,
            sessions_dir,
        };

        match presence {
            None => {
                let mut prepared = prepared;
                let active = prepared.begin(game, measure, &status)?;
                Ok(Recorder {
                    commands: Arc::new(Mutex::new(Some(active.commands))),
                    thread: Mutex::new(Some(active.thread)),
                    supervisor: Mutex::new(None),
                    status,
                    mode,
                    mic,
                })
            }
            Some((changes, watch)) => {
                let commands: Arc<Mutex<Option<Sender<Command>>>> = Arc::new(Mutex::new(None));
                let supervisor = spawn_supervisor(
                    prepared,
                    changes,
                    watch,
                    Arc::clone(&commands),
                    Arc::clone(&status),
                )?;
                Ok(Recorder {
                    commands,
                    thread: Mutex::new(None),
                    supervisor: Mutex::new(Some(supervisor)),
                    status,
                    mode,
                    mic,
                })
            }
        }
    }

    /// What the engine is doing right now. Never blocks the recording loop.
    pub fn status(&self) -> RecorderStatus {
        let status = &self.status;
        let error = status.error.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        RecorderStatus {
            running: status.running.load(Ordering::Relaxed),
            frames: status.frames.load(Ordering::Relaxed),
            segments: status.segments.load(Ordering::Relaxed),
            bytes: status.bytes.load(Ordering::Relaxed),
            span_ms: status.span_ms.load(Ordering::Relaxed),
            dropped: status.dropped.load(Ordering::Relaxed),
            dropped_audio: status.dropped_audio.load(Ordering::Relaxed),
            dropped_mic_audio: status.dropped_mic.load(Ordering::Relaxed),
            mic: self.mic,
            // 0 is the "no microphone input" sentinel the writer stores; a port is never 0.
            mic_port: match status.mic_port.load(Ordering::Relaxed) {
                0 => None,
                port => Some(port as u16),
            },
            mode: self.mode,
            game: status.game(),
            watching_games: status.watching.load(Ordering::Relaxed),
            skipped: status.skipped.load(Ordering::Relaxed),
            fps: f64::from_bits(status.fps.load(Ordering::Relaxed)),
            configured_fps: status.configured_fps.load(Ordering::Relaxed) as u32,
            effective_fps: status.effective_fps.load(Ordering::Relaxed) as u32,
            drift_ms: status.drift_ms.load(Ordering::Relaxed),
            clips: status.clips.load(Ordering::Relaxed),
            error,
        }
    }

    /// Take a clip now: wait for the post-roll, splice it losslessly, index it.
    ///
    /// The trigger instant is the **ledger's** media time, taken inside the recording
    /// loop at the moment the trigger is handled — not a wall-clock instant sampled here.
    /// That is the hard-won part: the ledger only counts finished segments, so a wall-clock
    /// trigger can name footage the encoder has not written yet — on the measured 4K box
    /// (media then running at 0.81x, issue #2) it sat beyond anything the ledger could reach
    /// and every press timed out.
    ///
    /// Blocks until the clip is written: the post-roll alone is `post_seconds` of media,
    /// and the wait covers it plus a margin. A caller that must stay responsive should run
    /// this off its own event loop (the desktop shell does).
    ///
    /// Equivalent to [`Recorder::clip_now_with`] with [`ClipReason::Manual`] — the hotkey's
    /// path, unchanged.
    pub fn clip_now(&self) -> Result<RecordedClip> {
        self.clip_now_with(ClipReason::Manual)
    }

    /// The same trigger, with the reason recorded.
    ///
    /// This is the single entry point for both manual and automatic clipping: a game event
    /// takes a clip through *this* call, so it inherits the media-time trigger instant, the
    /// post-roll wait and the lossless splice rather than reimplementing any of them. What
    /// the reason adds is one `events` row (spec §5.5) naming what happened, written after
    /// the clip is indexed and linked to it.
    pub fn clip_now_with(&self, reason: ClipReason) -> Result<RecordedClip> {
        let (reply, answer) = mpsc::channel();
        self.command_sender()?
            .send(Command::Clip { reason, reply })
            .map_err(|_| anyhow::anyhow!("the recorder stopped before it could take the clip"))?;
        // Cannot hang: the loop answers every queued command, and if it stops first its
        // receiver is dropped — which drops this command and its reply channel, making
        // this a disconnection error rather than a wait.
        answer
            .recv()
            .map_err(|_| anyhow::anyhow!("the recorder stopped before it could take the clip"))?
    }

    /// Record a derived event that did not ask for a clip, and return its `events` row id.
    ///
    /// Markers — a game starting or ending, a round boundary (see [`EventKind::is_highlight`])
    /// — are part of what happened in a session, so the session timeline should show them;
    /// they are not worth `pre_seconds` of footage each, so no clip is taken. The row's `at`
    /// is the trigger instant's position on the ledger timeline, the same clock a clip's
    /// `started_at` is on, so a marker and a clip can be compared directly. `clip_id` is
    /// NULL: no clip produced this.
    ///
    /// Blocking, and cheap — a single INSERT on the loop thread.
    pub fn note_event(&self, event: GameEvent) -> Result<i64> {
        let (reply, answer) = mpsc::channel();
        self.command_sender()?
            .send(Command::Note { event, reply })
            .map_err(|_| anyhow::anyhow!("the recorder stopped before it could note the event"))?;
        answer
            .recv()
            .map_err(|_| anyhow::anyhow!("the recorder stopped before it could note the event"))?
    }

    /// The command channel of the recording that is running **now**, or the error a caller
    /// gets when there is none.
    ///
    /// Two states answer with an error, and they are different states: a recorder that has
    /// stopped, and a game-driven recorder that is armed and waiting for a game
    /// ([`Recorder::is_armed`]). Both mean there is no footage to clip, and the message says
    /// which — a caller that assumed "running" would otherwise splice a clip out of a
    /// recording that does not exist.
    fn command_sender(&self) -> Result<Sender<Command>> {
        let guard = self.commands.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_ref() {
            Some(sender) => Ok(sender.clone()),
            None if self.is_armed() => Err(anyhow::anyhow!(
                "no recording is running: this recorder is watching for a game to start \
                 ([games] auto_record = true), so there is nothing to clip yet"
            )),
            None => Err(anyhow::anyhow!("the recorder is not running")),
        }
    }

    /// Stop recording: flush the encoder, close the capture and audio sources, join the
    /// loop — and, in [full-session mode](RecordingMode::FullSession), concatenate the
    /// session's segments into its file and close its `sessions` row first.
    ///
    /// Idempotent — a second call is a no-op that reports the same outcome — and safe
    /// from a shared `Arc<Recorder>`, because a stop that runs concurrently with another
    /// waits for the shutdown to finish rather than returning early.
    ///
    /// In games mode this also stops the watcher: with nothing watching, an armed recorder
    /// has nothing left to do. It is deliberately the same call for both modes, so a
    /// front-end that has one "stop" button does not have to know which mode it started.
    ///
    /// Returns the failure the loop stopped for, if it stopped for one — including a session
    /// that could not be concatenated, whose segments are kept and whose row is left open for
    /// the next start to finish (see [`session::recover_sessions`]).
    pub fn stop(&self) -> Result<()> {
        // The supervisor first: it is what stops a game-driven recording, and it joins that
        // recording's thread itself. Taking the handle under the lock and joining outside it
        // keeps this callable from two threads at once, with the second waiting for the same
        // join rather than racing it.
        let supervisor = self.supervisor.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(supervisor) = supervisor {
            supervisor.shutdown();
        }

        // Then a recording this `Recorder` started itself (or the one the supervisor just
        // stopped: its sender is gone, so this is a no-op). The guard is held across the
        // join, so two concurrent stops cannot both see the handle and one of them return
        // before the thread is gone.
        let mut guard = self.thread.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(thread) = guard.take() {
            // A disconnected loop has already stopped; `send` failing is not an error.
            let command = self.commands.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if let Some(sender) = command {
                let _ = sender.send(Command::Stop);
            }
            if thread.join().is_err() {
                return Err(anyhow::anyhow!("the recording thread panicked"));
            }
        }
        *self.commands.lock().unwrap_or_else(|p| p.into_inner()) = None;

        let status = self.status();
        status.error.map_or(Ok(()), |why| Err(anyhow::anyhow!(why)))
    }

    /// Whether the loop thread is still running — a cheap check for a driver's loop.
    ///
    /// `false` for an armed game-driven recorder that is waiting for a game: it is not
    /// recording, and [`Recorder::is_armed`] is the other half of the answer.
    pub fn is_running(&self) -> bool {
        self.status.running.load(Ordering::Relaxed)
    }

    /// Whether this recorder is armed: a game watcher is running and a recording will begin
    /// when a watched game does. Always `false` unless [`RecorderOptions::games`] had
    /// `auto_record` on at start.
    pub fn is_armed(&self) -> bool {
        self.status.watching.load(Ordering::Relaxed)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Never leave a capture session and an ffmpeg child behind a dropped handle. The
        // recorder ignores the result: a drop has nowhere to report an error to, and the
        // failure (if any) is already in the status for whoever reads it next.
        if let Err(err) = self.stop() {
            tracing::warn!("the recorder did not shut down cleanly: {err:#}");
        }
    }
}

/// How long the supervisor waits for a presence change before checking its stop flag.
///
/// Bounds how long a [`Recorder::stop`] can be held by a watcher that has nothing to report:
/// the loop wakes at least this often.
const SUPERVISOR_TICK: Duration = Duration::from_millis(200);

/// One microphone block, in ms — the pipeline's convention, and the length
/// `MICROPHONE_FORMAT` and the encoder's `MicAudioSpec` both describe.
const MIC_BLOCK_MS: u64 = 10;

/// Everything a start resolved **before** a recording begins, and everything a second
/// recording needs in order to begin: a game watcher records one session per game, and each
/// repeats the handshake that does not touch the display.
struct Prepared {
    cfg: RecorderConfig,
    opts: RecorderOptions,
    codec: VideoCodec,
    vendor: Option<Vendor>,
    /// Taken by a recording when it begins and given back when it ends: one SQLite
    /// connection, moved between the engine's thread and — in games mode — the supervisor's.
    /// (`rusqlite::Connection` is `Send` and not `Sync`, so it is moved rather than shared.)
    store: Option<Store>,
    /// The shared scratch directory (ring mode's segments, and where the clip path looks).
    scratch_dir: PathBuf,
    /// The clips directory both modes write clips into.
    clips_dir: PathBuf,
    /// The sessions area: session mode's segment directories and concatenated files.
    sessions_dir: PathBuf,
}

/// A recording that is running: where its commands go, and the thread that runs it.
struct Active {
    commands: Sender<Command>,
    /// The engine thread, which hands the store back when it ends.
    thread: JoinHandle<Store>,
}

impl Prepared {
    /// The startup handshake for one recording — steps 4 to 7 of
    /// [`Recorder::start_with_options`]'s list, in the same order and for the same reasons.
    ///
    /// `game` is the watched game that triggered this recording, written into the session
    /// row; `measure` is the throughput probe (the caller's, for a recording `start` begins
    /// itself; the shipping one, when a watcher begins it later on its own thread).
    fn begin(
        &mut self,
        game: Option<String>,
        measure: &dyn Fn(&EncodeConfig) -> Result<ThroughputMeasurement>,
        status: &Arc<SharedStatus>,
    ) -> Result<Active> {
        // Cloned once per recording: a `RecorderConfig` is a handful of paths and small
        // numbers, and a local copy keeps the borrow checker out of the rest of this function.
        let cfg = self.cfg.clone();
        let mode = self.opts.mode;
        let store = self
            .store
            .take()
            .context("this recorder's store is already in a recording")?;
        let buffer_cfg = BufferConfig {
            pre_ms: cfg.buffer.pre_seconds * 1000,
            post_ms: cfg.buffer.post_seconds * 1000,
            scratch_cap_bytes: cfg.buffer.scratch_cap_bytes,
            segment_ms: cfg.buffer.segment_time * 1000,
            clips_dir: self.clips_dir.clone(),
        };

        // The directory this recording's segments go into. Ring mode: the shared scratch
        // directory. Session mode: a directory of its own under the sessions area, named
        // from the wall clock and created by the attempt (`create_dir`, which fails rather
        // than overwrites), so two processes starting in the same second cannot mix their
        // footage into one session file.
        // One wall-clock instant decides the session row's `started_at`, the directory's
        // name and (at stop) the file's: `now_ms()` is read once, here.
        let session_started_ms = now_ms();
        let segment_dir = if mode.is_full_session() {
            session::create_session_dir(&self.sessions_dir, session_started_ms)?
        } else {
            self.scratch_dir.clone()
        };
        // A new recording's failure state is its own: last game's error must not make this
        // one look failed (a watcher records many sessions through one `Recorder`).
        *status.error.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;

        // The microphone is created **before** the capture backend exists: on Windows
        // creating the video backend opens a Windows Graphics Capture session on the user's
        // primary monitor, and a recording that cannot start its microphone must not have
        // touched the display first. This is the fail-loud half of `[mic] enabled` — an
        // encoder configured with a microphone input that nothing feeds would record a
        // silent voice track while claiming one.
        let mut mic = if self.opts.mic.enabled {
            Some(build_microphone(cfg.sources).with_context(|| {
                "mic.enabled = true, but the microphone backend could not be created; \
                 disable the microphone or fix the capture device"
            })?)
        } else {
            None
        };
        let microphone = mic.is_some();

        // Only now create the capture backend: the encoder's rawvideo pipe is declared
        // from the backend's own frame geometry (`native_size` — the monitor under WGC,
        // the configured size for the stub), so the size-dependent half of the encoder
        // config has to wait until the backend exists.
        let (mut capture, mut audio) = build_sources(cfg.sources, cfg.encode.fps)?;
        let native = capture.native_size();

        let mut encode_cfg = build_encode_config(&cfg, &segment_dir, self.codec, self.vendor, native)?;
        if microphone {
            // Turn the encoder's second audio input on. `MicAudioSpec::default()` is the
            // canonical 48kHz stereo s16le — the same format `MICROPHONE_FORMAT` publishes and
            // the same one the game-audio input has always been declared with — so the two
            // tracks are declared by one rule and cannot drift apart.
            encode_cfg.mic_audio = Some(MicAudioSpec::default());
        }

        // The rate decision, and the point where the pipeline stops declaring a rate it
        // cannot deliver. The probe (when it runs) is given the configuration that declares
        // the *configured* rate — that is part of the invocation being measured — and the
        // rate it reports is what the whole pipeline then uses.
        let decision = if cfg.encode.adapt_fps {
            let measurement = measure(&encode_cfg).with_context(|| {
                format!(
                    "measuring the sustainable encode rate with {} at {}x{}",
                    encode_cfg.encoder_name(),
                    native.0,
                    native.1
                )
            })?;
            FpsDecision::decide(cfg.encode.fps, true, Some(measurement))
        } else {
            FpsDecision::decide(cfg.encode.fps, false, None)
        };
        encode_cfg.fps = decision.effective();
        log_rate_decision(&decision, encode_cfg.encoder_name(), native);

        // The ledger is built and adopted *before* the encoder is spawned because the
        // segment number the encoder must continue from is decided from what is already
        // on disk, and that number is one of ffmpeg's arguments.
        let mut ledger = match mode {
            RecordingMode::ReplayBuffer => Ledger::Buffer(RingBuffer::start(
                &cfg.bin,
                buffer_cfg.clone(),
                self.scratch_dir.clone(),
                encode_cfg.encoder_name().to_string(),
            )?),
            RecordingMode::FullSession => Ledger::Session(session::SessionRing::open(
                &cfg.bin,
                &segment_dir,
                &buffer_cfg,
                microphone,
                encode_cfg.encoder_name().to_string(),
            )?),
        };
        let adopted = ledger.adopt_existing()?;
        if adopted > 0 {
            tracing::info!("adopted {adopted} segments from a previous run");
        }
        encode_cfg.start_number = ledger.reserve_number()?;
        if encode_cfg.start_number > 0 {
            tracing::info!(
                "segment numbering continues at {} (previous material is on disk)",
                encode_cfg.start_number
            );
        }

        let (encoder, encoder_name) = spawn_encoder(&cfg.bin, &encode_cfg)?;
        tracing::info!("encoding with {encoder_name}");
        // The pacer's rate is read back out of the encoder object — i.e. out of the
        // configuration ffmpeg was actually spawned with — instead of being taken from
        // `encode_cfg.fps` a second time. That is the structural half of the fix: the pacer
        // cannot pace to a number the encoder child was not told, whichever way a future edit
        // rearranges the code around it, and the published rate is then the child's own
        // number rather than a belief about it. Read here, before the encoder is moved into
        // the engine.
        let encoder_fps = encoder.input_fps();
        // The microphone's second check: the encoder must actually have opened the second
        // input. `mic_audio` being set and a port existing are two halves of one fact, and a
        // mismatch means the recording would claim a track nothing feeds.
        let mic_port = encoder.mic_port();
        if microphone && mic_port.is_none() {
            bail!(
                "the encoder was spawned without a microphone input, so this recording would \
                 carry a voice track nothing feeds (the encode configuration asked for one)"
            );
        }
        // One-line capture-geometry summary so a reader can see the resolution being
        // captured and that the frame counter starts from zero (criterion 1). The rate is
        // the one the pipeline is running at; when it is below the configured rate, the
        // adaptation line above says so and why.
        tracing::info!(
            "capture geometry {}x{} at {}fps (frame counter starts at 0){}",
            native.0,
            native.1,
            encoder_fps,
            if microphone { format!(", plus a microphone track on port {mic_port:?}") } else { String::new() }
        );
        if microphone && !mode.is_full_session() {
            // The ring's trigger is `localplay_replay`'s and splices a clip with
            // `ClipSplicer::splice`, whose concat has no `-map`: one audio stream survives it.
            // A session's own trigger does not have that limit (this crate builds the concat
            // for it — see `session::finalise`), so this is a buffer-mode limitation, and the
            // user is told before pressing the hotkey rather than after.
            tracing::warn!(
                "the microphone track is being recorded, but a clip spliced out of the replay \
                 buffer will carry only the game audio: the clip concatenation selects one \
                 audio stream (it has no `-map`). Record in session mode ([recorder] \
                 mode = \"session\") to keep the microphone in the clip's file."
            );
        }

        // The session row, before anything is captured: a crash mid-recording then always
        // leaves a row for the next start to find, and the retention rules manage the segment
        // directory through this row (`scratch_dir`, `size_bytes`, `ended_at`). Its
        // `started_at` is the instant read above, which also named the directory.
        let session = match store.start_session(
            game.as_deref(),
            mode.store_mode(),
            session_started_ms,
            &segment_dir.display().to_string(),
        ) {
            Ok(id) => id,
            Err(err) => {
                // Nothing has been captured; the directory that was created for it goes.
                if mode.is_full_session() {
                    session::remove_session_dir(&segment_dir);
                }
                return Err(err).context("opening the session row");
            }
        };
        tracing::info!(
            "session #{session} opened ({}){}: segments in {}{}",
            mode,
            match &game {
                Some(game) => format!(", game {game:?}"),
                None => String::new(),
            },
            segment_dir.display(),
            if mode.is_full_session() { " (the scratch cap does not apply)" } else { "" }
        );

        // Start the sources. The microphone first — it is the input whose failure the user
        // cannot see in the picture — then the display, then the game audio; on any failure
        // everything already started is stopped again, and the session row is closed so
        // nothing is left half-open.
        let started = (|| -> Result<()> {
            if let Some(mic) = mic.as_mut() {
                mic.start().context("starting the microphone")?;
            }
            capture.start().context("starting screen capture")?;
            audio.start().context("starting audio capture")?;
            Ok(())
        })();
        if let Err(err) = started {
            if let Some(mic) = mic.as_mut() {
                let _ = mic.stop();
            }
            let _ = capture.stop();
            let _ = audio.stop();
            let _ = store.end_session(session, now_ms(), None, 0);
            if mode.is_full_session() {
                session::remove_session_dir(&segment_dir);
            }
            return Err(err);
        }

        let (commands, inbox) = mpsc::channel();
        let engine = Engine {
            pre_ms: buffer_cfg.pre_ms,
            post_ms: buffer_cfg.post_ms,
            scratch_cap_bytes: buffer_cfg.scratch_cap_bytes,
            storage: cfg.storage.clone(),
            cleanup: CleanupReport::default(),
            // The pacer paces to the rate the encoder child was told — read back from the
            // encoder itself, so the two cannot disagree. See `Recorder::start_with_measure`:
            // that agreement is the whole fix.
            pacer: FramePacer::new(encoder_fps),
            capture,
            audio,
            mic,
            microphone,
            encoder,
            ledger,
            mode,
            session,
            session_started_ms,
            sessions_dir: self.sessions_dir.clone(),
            size_warning_reported: false,
            encoder_name,
            store,
            clock: CaptureClock::new(),
            status: Arc::clone(status),
            rate: RateMeter::new(RATE_WINDOW),
            configured_fps: decision.configured(),
            effective_fps: encoder_fps,
            frames: 0,
            skipped: 0,
            achieved: 0.0,
            last_counted_dropped: 0,
            last_warned_dropped: 0,
            last_drop_warning: None,
        };
        status.publish_recording(decision.configured(), encoder_fps, mic_port);
        status.set_game(game);
        status.running.store(true, Ordering::SeqCst);

        let thread = std::thread::Builder::new()
            .name("localplay-recorder".to_string())
            .spawn(move || engine.run(inbox))
            .context("spawning the recording thread")?;

        Ok(Active { commands, thread })
    }
}

/// The game watcher's driver: one recording per game session, in the configured mode.
///
/// It owns the presence changes, the watcher's handle (which must stay alive for the watcher
/// to keep running), the prepared start state, and the recording that is running — if there
/// is one. Everything about *detection* belongs to `localplay-events` (including its
/// two-poll debounce); everything about *what a detection means for recording* is here.
struct Supervisor {
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Supervisor {
    /// Ask the loop to stop and wait for it — including for the recording it stops on the way
    /// out. Takes `&self` so [`Recorder::stop`] can call it through the mutex, twice if a
    /// caller stops a recorder twice.
    fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
            // A poll in flight finishes (it is bounded by its own tick), and the join is what
            // makes "stopped" mean it.
            let _ = thread.join();
        }
    }
}

/// Start the supervisor thread (see [`Supervisor`]).
///
/// The channel and the watcher both come from the caller: production's are
/// `GamesSection::start`'s, a test's are its own (which is how the game start/stop path is
/// exercised without a process list or a Live Client API).
fn spawn_supervisor(
    prepared: Prepared,
    presence: Receiver<PresenceChange>,
    watch: Option<WatchHandle>,
    commands: Arc<Mutex<Option<Sender<Command>>>>,
    status: Arc<SharedStatus>,
) -> Result<Supervisor> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    // Armed the moment this returns: a front-end that asks "is this recorder watching?" right
    // after `Recorder::start` gets the answer, not a race with a thread that has not run yet.
    status.watching.store(true, Ordering::SeqCst);
    let thread = std::thread::Builder::new()
        .name("localplay-games".to_string())
        .spawn(move || {
            // Kept alive for the whole loop: dropping the handle stops the watcher, which is
            // the one thing this thread cannot live without.
            let _watch = watch;
            let mut prepared = prepared;
            tracing::info!(
                "watching for a game to start: recording in {} mode when one does, and \
                 nothing before that (games.auto_record = true)",
                prepared.opts.mode
            );

            let mut running: Option<(String, JoinHandle<Store>)> = None;
            while !flag.load(Ordering::SeqCst) {
                match presence.recv_timeout(SUPERVISOR_TICK) {
                    Ok(PresenceChange::Started(game)) => {
                        if let Some((name, _)) = &running {
                            tracing::debug!(
                                "{game} started while {name} is being recorded; the recording \
                                 that is already running keeps the machine"
                            );
                            continue;
                        }
                        // The shipping measurement, because this recording begins on this
                        // thread: a caller's injected probe belongs to the recording
                        // `Recorder::start` begins itself.
                        let bin = prepared.cfg.bin.clone();
                        let measure = move |encode_cfg: &EncodeConfig| {
                            localplay_encoder::throughput::measure_sustainable_fps(
                                &bin,
                                encode_cfg,
                                localplay_encoder::throughput::PROBE_BUDGET,
                            )
                        };
                        match prepared.begin(Some(game.name.clone()), &measure, &status) {
                            Ok(active) => {
                                tracing::info!(
                                    "{game} started: recording a {} session (stop the \
                                     recording by quitting the game, or by stopping this \
                                     application)",
                                    prepared.opts.mode
                                );
                                *commands.lock().unwrap_or_else(|p| p.into_inner()) =
                                    Some(active.commands.clone());
                                running = Some((game.name, active.thread));
                            }
                            Err(err) => {
                                // Reported, and the watcher keeps watching: the next game may
                                // well record, and a machine that cannot encode has already
                                // been told so by the startup check.
                                tracing::error!(
                                    "{game} started, but the recording could not start: {err:#}"
                                );
                                *status.error.lock().unwrap_or_else(|p| p.into_inner()) =
                                    Some(format!("{err:#}"));
                            }
                        }
                    }
                    Ok(PresenceChange::Stopped(game)) => {
                        let Some((name, thread)) = running.take() else { continue };
                        if name != game.name {
                            // A game that was not the one being recorded stopped; the
                            // recording continues.
                            running = Some((name, thread));
                            continue;
                        }
                        tracing::info!("{game} stopped: closing the recording");
                        // Clearing the command channel drops the engine's last sender, which
                        // ends its loop; the join waits for the shutdown that does the work —
                        // the encoder flush, the session file, the row's end.
                        *commands.lock().unwrap_or_else(|p| p.into_inner()) = None;
                        match thread.join() {
                            Ok(store) => {
                                prepared.store = Some(store);
                                // The retention pass between game sessions (spec §8.1): the
                                // session that just ended is the one it now manages.
                                let mut report = CleanupReport::default();
                                if let Some(store) = &prepared.store {
                                    cleanup_pass(store, &prepared.cfg.storage, &mut report);
                                }
                            }
                            Err(_) => tracing::error!(
                                "the recording thread for {game} panicked; its session row is \
                                 left for the next start to recover"
                            ),
                        }
                        status.set_game(None);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    // Nothing is watching any more: rather than capture for nobody, stop.
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }

            // Leaving: a recording that is still running (the application is shutting down)
            // is stopped, so nothing keeps capturing.
            if let Some((name, thread)) = running.take() {
                tracing::info!("stopping the recording of {name}: the recorder is shutting down");
                *commands.lock().unwrap_or_else(|p| p.into_inner()) = None;
                if let Ok(store) = thread.join() {
                    prepared.store = Some(store);
                }
            }
            status.watching.store(false, Ordering::SeqCst);
            status.set_game(None);
            status.running.store(false, Ordering::SeqCst);
        })
        .context("spawning the game supervisor thread")?;
    Ok(Supervisor { stop, thread: Mutex::new(Some(thread)) })
}

/// The microphone backend for the configured sources.
///
/// The same rule the video and audio backends follow (`localplay_capture::platform`): with
/// [`Sources::Platform`] it is the platform's own backend — real WASAPI on Windows, and a
/// clear **error** everywhere else, because there is no microphone backend off Windows and a
/// synthetic stand-in selected at runtime would make the recorder look like it is capturing a
/// voice track while recording nothing — and with [`Sources::Stub`] it is
/// [`StubMicrophone`], which is what lets the whole microphone path be exercised on a machine
/// with no WASAPI (the crate's tests, CI, and the CLI's `--dev-stub-sources`).
fn build_microphone(sources: Sources) -> Result<Box<dyn AudioBackend>> {
    match sources {
        Sources::Stub(_) => Ok(Box::new(StubMicrophone::new(MICROPHONE_FORMAT, MIC_BLOCK_MS))),
        Sources::Platform => localplay_capture::wasapi_mic::microphone_backend(),
    }
}

/// Say what the crash-recovery pass found, in the log's own terms.
///
/// The detail lines are `recover_sessions`'s; this is the one line that says whether the pass
/// changed anything at all, so a start is never silent about finding a previous run's
/// footage.
fn log_recovery(recovery: &session::Recovery) {
    if recovery.is_empty() {
        return;
    }
    tracing::info!(
        "crash recovery: {} session(s) finalised, {} closed with no footage, {} left \
         unfinalised (see above), {} leftover directory/directories, {} unnamed director(ies)",
        recovery.recovered.len(),
        recovery.closed_empty.len(),
        recovery.unfinalised.len(),
        recovery.leftover_dirs.len(),
        recovery.orphan_dirs.len()
    );
}

/// The capture loop's state: the ledger, the encoder, the pacer, and the counters.
struct Engine {
    pre_ms: u64,
    post_ms: u64,
    scratch_cap_bytes: u64,
    storage: StorageSection,
    cleanup: CleanupReport,
    pacer: FramePacer,
    capture: Box<dyn CaptureBackend>,
    audio: Box<dyn AudioBackend>,
    /// The microphone backend, when this recording has a microphone track. Drained by the
    /// same pump iteration as the game audio, into the encoder's own second input queue —
    /// one writer thread per input, which is why this is a second backend and not a second
    /// write from this thread. Taken (stopped) during shutdown, which is why the *fact* that
    /// this recording has a microphone is a field of its own ([`Engine::microphone`]).
    mic: Option<Box<dyn AudioBackend>>,
    /// Whether this recording has a microphone track. Read by the shutdown path — which runs
    /// after [`Engine::mic`] has been taken and stopped — to decide how the session's
    /// segments and a clip's segments are concatenated (with a second audio track they need
    /// `-map 0`; without it, the splicer's own concat is used).
    microphone: bool,
    encoder: Box<dyn Encoder>,
    /// Where the segments live and how the trigger finds them: the replay ring (which
    /// evicts to the cap) or the session's own segment store (which never evicts).
    ledger: Ledger,
    /// The mode this recording runs in ([`RecordingMode`]).
    mode: RecordingMode,
    /// The `sessions` row this recording opened. Always `Some` in practice: a recording
    /// with no row would be footage nothing can manage.
    session: i64,
    /// When the session started, on the wall clock (`sessions.started_at`), kept as an i64
    /// ms epoch value for the session file's name and the row's timestamps.
    session_started_ms: i64,
    /// The sessions area, where a session's file is written at stop.
    sessions_dir: PathBuf,
    /// Whether a failed `set_session_size` has been reported already: the tick that makes
    /// the number current runs every [`STATUS_INTERVAL`], and a database that cannot take
    /// the update is worth one line, not five per second.
    size_warning_reported: bool,
    /// The name of the encoder that actually ran (`h264_nvenc`, `libx264`, …), recorded on
    /// the clips and the session file this engine writes (spec §11 criterion 5).
    encoder_name: String,
    store: Store,
    clock: CaptureClock,
    status: Arc<SharedStatus>,
    rate: RateMeter,
    /// Video frames submitted to the encoder, including the ones the post-roll wait of a
    /// trigger submitted (`frames=` on the status line).
    frames: u64,
    /// Frames the backend offered and the pacer threw away *without* the readback
    /// ([`CaptureBackend::discard_pending`]). Logged next to `frames=` because the two
    /// together are the whole accounting of what the source produced, and because a
    /// non-zero `skipped=` next to a `frames=` holding the configured rate is the
    /// signature of this optimisation working: the surplus is being skipped cheaply
    /// instead of being copied and then dropped.
    skipped: u64,
    /// What `encode.fps` asked for. Reported (`RecorderStatus::configured_fps`), and used
    /// in the drop warning to say *why* the rate in use may be lower than requested —
    /// never to pace or to configure the encoder, which is [`Engine::effective_fps`]'s job.
    configured_fps: u32,
    /// The rate the pipeline is actually running at: the pacer's interval and the encoder
    /// child's `-framerate`. The status line's denominator is this value, and the drop warning
    /// compares the achieved rate against it, because "the encoder cannot sustain the rate it
    /// was told" is the condition that warning exists for — frames captured and never encoded,
    /// which the picture holds through.
    effective_fps: u32,
    /// The last rate a window closed on, published on every pump and logged on every tick.
    achieved: f64,
    /// The dropped count as of the last time each of the two consumers looked at it: the
    /// rate meter ([`Engine::last_counted_dropped`]) and the warning
    /// ([`Engine::last_warned_dropped`]). Both are cumulative counters, so each drop must
    /// be subtracted exactly once.
    last_counted_dropped: u64,
    /// The dropped count as of the last warning, so "still dropping" can be told from "was
    /// dropping once, an hour ago".
    last_warned_dropped: u64,
    last_drop_warning: Option<Instant>,
}

/// The segments a recording's trigger and shutdown work against.
///
/// Two shapes, one timeline: the replay ring evicts to `buffer.scratch_cap_bytes` when it
/// scans (it is a bounded window by definition), and the full session's own store never
/// evicts — a whole session is footage that was asked for, so the cap does not apply to it
/// at all ([`crate::session`] explains why that is structural rather than a large cap).
enum Ledger {
    /// The rolling replay buffer.
    Buffer(RingBuffer),
    /// A full session's segment directory.
    Session(crate::session::SessionRing),
}

impl Ledger {
    /// Find newly written segments (the buffer's scan evicts; the session's never does).
    fn scan(&mut self) -> Result<()> {
        match self {
            Ledger::Buffer(ring) => ring.scan_once(),
            Ledger::Session(session) => session.scan(),
        }
    }

    /// Index every segment in the directory, the newest file included. Called at shutdown,
    /// after the encoder has been flushed and its child has exited, so the last file is
    /// complete and must not be dropped (the ring's live scan deliberately does not trust
    /// the newest file while ffmpeg is still appending to it).
    fn scan_all(&mut self) -> Result<()> {
        match self {
            Ledger::Buffer(ring) => ring.scan_once(),
            Ledger::Session(session) => session.scan_all(),
        }
    }

    /// What is on disk, in the shape the status line and the scratch cap use.
    fn stats(&self) -> BufferStats {
        match self {
            Ledger::Buffer(ring) => ring.stats(),
            Ledger::Session(session) => BufferStats {
                segments: session.segment_count(),
                bytes_on_disk: session.bytes_on_disk(),
                span_ms: session.span_ms(),
            },
        }
    }

    /// Run-relative media time on disk, in ms.
    fn span_ms(&self) -> u64 {
        self.stats().span_ms
    }

    /// The ledger's own position of this run's zero.
    fn origin_ms(&self) -> u64 {
        match self {
            Ledger::Buffer(ring) => ring.ledger_origin_ms(),
            Ledger::Session(session) => session.origin_ms(),
        }
    }

    /// The pump's view of this ledger, for the post-roll wait.
    fn as_ring(&mut self) -> &mut dyn MediaRing {
        match self {
            Ledger::Buffer(ring) => ring,
            Ledger::Session(session) => session,
        }
    }

    /// Index what is already on disk, before the encoder is spawned (the segment number it
    /// is told depends on this).
    fn adopt_existing(&mut self) -> Result<usize> {
        match self {
            Ledger::Buffer(ring) => ring.adopt_existing(),
            Ledger::Session(session) => session.adopt_existing(),
        }
    }

    /// Reserve the sequence number the encoder must start writing at.
    fn reserve_number(&mut self) -> Result<u64> {
        match self {
            Ledger::Buffer(ring) => ring.reserve_segment_number(),
            Ledger::Session(session) => session.reserve_number(),
        }
    }

    /// Whether this ledger's scan enforces the scratch cap (the buffer) or never deletes
    /// anything (a session).
    fn evicts_to_cap(&self) -> bool {
        matches!(self, Ledger::Buffer(_))
    }

    /// Splice a clip around `trigger_ms` out of this ledger's footage.
    fn trigger(&self, trigger_ms: u64, stem: &str) -> Result<ClipMetadata> {
        match self {
            Ledger::Buffer(ring) => ring.trigger(trigger_ms, stem).map_err(Into::into),
            Ledger::Session(session) => session.trigger(trigger_ms, stem),
        }
    }
}

impl Engine {
    /// The pump loop. Returns when it is told to stop, when the last `Recorder` is
    /// dropped, or when the capture/encode path fails — and hands the store back, because a
    /// game-driven recorder opens the *next* recording with the same connection.
    ///
    /// Everything the recording writes is closed before this returns: the encoder flushed,
    /// the sources stopped, the `sessions` row ended and (in full-session mode) the session
    /// file written.
    fn run(mut self, commands: Receiver<Command>) -> Store {
        let mut last_scan = Instant::now();
        // Next storage-policy pass. Measured in wall clock from the last one, because the
        // policy is about a directory that fills at whatever rate the user clips.
        let mut last_cleanup = Instant::now();

        loop {
            match commands.try_recv() {
                Ok(Command::Stop) => break,
                Ok(Command::Clip { reason, reply }) => {
                    let taken = self.take_clip(reason);
                    // The caller may have given up (a window closed, a test finished);
                    // that is its business, not the loop's.
                    let _ = reply.send(taken);
                }
                Ok(Command::Note { event, reply }) => {
                    let noted = self.note_event(&event);
                    let _ = reply.send(noted);
                }
                Err(TryRecvError::Empty) => {}
                // Every `Recorder` was dropped: nothing can ask for a clip any more, so
                // the loop stops instead of capturing for nobody.
                Err(TryRecvError::Disconnected) => break,
            }

            if let Err(err) = self.pump() {
                self.fail(err);
                break;
            }

            if last_scan.elapsed() >= STATUS_INTERVAL {
                if let Err(err) = self.tick(&mut last_cleanup) {
                    self.fail(err);
                    break;
                }
                last_scan = Instant::now();
            }
        }

        // Flush and shut down. On the failure path this is best effort: the encoder may
        // be the thing that failed, and its `finish` reports that rather than blocking.
        if let Err(err) = self.shutdown() {
            self.fail(err);
        }
        self.status.running.store(false, Ordering::SeqCst);
        self.store
    }

    /// One pump: submit whatever frame and audio are due.
    fn pump(&mut self) -> Result<()> {
        let counts = pump_once_counted_with_mic(
            &mut self.pacer,
            self.capture.as_mut(),
            self.audio.as_mut(),
            self.mic.as_deref_mut(),
            self.encoder.as_mut(),
        )?;
        self.account(counts);
        Ok(())
    }

    /// The ~200ms tick: scan the ledger, publish and log the status, keep the session row's
    /// size current, enforce the scratch cap (ring mode only), warn about a starving
    /// encoder, and apply the storage policy on its own interval.
    fn tick(&mut self, last_cleanup: &mut Instant) -> Result<()> {
        self.ledger.scan().context("scanning for new segments")?;
        if let Ledger::Buffer(ring) = &self.ledger {
            ring.save_ledger()?;
        }

        let stats = self.ledger.stats();
        self.status.publish_ring(
            stats.segments as u64,
            stats.bytes_on_disk,
            stats.span_ms,
        );
        // The `sessions` row's size, on the tick that already measured it: a multi-hour
        // recording has to be visible to the sessions retention cap *before* it stops
        // (spec §8.1).
        self.report_session_size(stats.bytes_on_disk);
        let dropped = self.encoder.dropped_frames();

        // `dropped=` is the encoder's own count of frames it had to discard because its
        // queue was full (it cannot slow the capture down, see the queue note in
        // `localplay-encoder`). It belongs next to `frames=` because the two together say
        // whether the pipeline is keeping up: a run that quietly loses a third of its
        // frames must not look like a clean one. `dropped_audio=` is the same count for
        // audio blocks; it is logged separately because a drop there is a hole in the
        // sound rather than a repeated picture, and the two have different acceptable rates.
        //
        // `skipped=` and `frames=` are one account: every frame the backend offered was
        // either submitted to the encoder or skipped without the readback, so `frames=` +
        // `skipped=` is the source's own rate. `fps=` is the achieved rate over the last
        // `RATE_WINDOW` — the frames that actually reached the encoder per second —
        // printed against the rate the pipeline is running at, so a shortfall is readable at
        // a glance instead of inferred from a counter's slope. `configured=` carries what
        // `encode.fps` asked for, which differs from that rate exactly when the startup
        // probe measured less and adaptation is on (the startup line says so in words).
        tracing::debug!(
            "frames={} segments={} bytes={} span={}ms dropped={} dropped_audio={} \
             dropped_mic={} skipped={} fps={:.1}/{} configured={}",
            self.frames,
            stats.segments,
            stats.bytes_on_disk,
            stats.span_ms,
            dropped,
            self.encoder.dropped_audio_blocks(),
            self.encoder.dropped_mic_audio_blocks(),
            self.skipped,
            self.achieved,
            self.effective_fps,
            self.configured_fps
        );
        // The scratch cap is the **ring's** rule (spec §8.1): a bounded window that overruns
        // its budget is a disk that fills up. A full session is not a window — its segments
        // are the recording the user asked for — so the check does not apply to it, and that
        // is structural (`evicts_to_cap`), not a cap set high enough to be missed.
        if self.ledger.evicts_to_cap() && stats.bytes_on_disk > self.scratch_cap_bytes {
            bail!(
                "scratch cap violated: {} bytes on disk exceeds {}",
                stats.bytes_on_disk,
                self.scratch_cap_bytes
            );
        }

        // A rising `dropped=` means the encoder's queue is overflowing while the pacer
        // admits at most the rate the pipeline is running at: the machine cannot encode that
        // rate *now* — after a startup measurement said it could, and after the pacer was
        // set to it. That must be loud rather than inferred, because it is a *capture* fact,
        // not a quality setting: every dropped frame is a moment of the recording that will
        // be held on the previous picture instead of shown.
        //
        // What it is NOT any more is a timeline warning. The media clock is the frames'
        // arrival timestamps (`-fps_mode passthrough`, see `localplay_encoder::ffmpeg`), so
        // the `span=` in the status line keeps tracking the wall clock and a `pre_seconds`
        // window is still that many real seconds even while this warning is firing: measured
        // on the dev host, a 4K output fed at ~45fps against a declared 120 (1172 of 1801
        // frames dropped by the queue) still wrote one 1s segment per second of wall clock
        // and a 3-segment clip covering ~2.8s of real footage. The warning is therefore
        // about the picture, and the wording below says so.
        let dropped_since_last = dropped.saturating_sub(self.last_warned_dropped);
        let warn_due = self
            .last_drop_warning
            .is_none_or(|t| t.elapsed() >= DROP_WARN_INTERVAL);
        // `achieved > 0.0` means a rate has been measured at all: before the first window
        // closes there is no number to quote, so the warning waits for one. Nothing is
        // lost by waiting — `dropped_since_last` stays non-zero until a warning is
        // actually emitted, which is also what keeps this from being printed once per
        // log line.
        if dropped_since_last > 0 && warn_due && self.achieved > 0.0 {
            tracing::warn!(
                "the encoder cannot sustain the {}fps it was told: only {:.1} frames per \
                 second are reaching it, and its queue is dropping the rest ({} since the \
                 last report, {dropped} in total) even though the pacer admits at most the \
                 same {}fps. The recording's clock is still the wall clock, so a clip still \
                 covers the seconds it says it does — but every dropped frame is a moment \
                 the picture will hold the previous frame through. Lower encode.fps, or set \
                 encode.output_size smaller.",
                self.effective_fps,
                self.achieved,
                dropped_since_last,
                self.effective_fps
            );
            self.last_drop_warning = Some(Instant::now());
            self.last_warned_dropped = dropped;
        }

        // Periodic storage-policy pass (spec §8.1), on the same tick as the scratch scan
        // that gates it. `cleanup_pass` is silent when there is nothing to do, which is
        // every pass in the ordinary case.
        if last_cleanup.elapsed() >= CLEANUP_INTERVAL {
            cleanup_pass(&self.store, &self.storage, &mut self.cleanup);
            *last_cleanup = Instant::now();
        }
        Ok(())
    }

    /// The trigger path (spec §6.2), run on the loop thread because it pumps.
    fn take_clip(&mut self, reason: ClipReason) -> Result<RecordedClip> {
        // The trigger is "now" on the LEDGER's timeline — media time — not on the wall
        // clock, and that is deliberate: the two clocks only agree while the pipeline
        // keeps up with `encode.fps`, and on real 4K hardware it does not. Measured on
        // the box (RTX 3090, 3840x2160): segments were written at 0.81/s while each one
        // contained exactly 1.000000s of media, so the media timeline advanced at ~0.81x
        // of the wall clock. A wall-clock `trigger_ms` made `need_ms` unreachable — the
        // ledger can never catch up to a target derived from a clock running ~19% ahead
        // of it — which is exactly the measured failure this replaces: `timed out ...
        // waiting for post-roll (span=27000ms need=29236ms)`. `span_ms` is the end of the
        // footage the ring can prove is on disk, i.e. the media-time position of "now";
        // `need_ms` and the splice window `[trigger_ms - pre_ms, trigger_ms + post_ms]`
        // are then all on that same clock, so the post-roll target is reachable by
        // construction.
        //
        // Consequence of measuring in media time: `pre_seconds` means seconds of *recorded
        // footage*, and a clip is spliced from whole segments, so it can run up to one
        // segment past the request but cannot fall short of it (the post-roll is waited for).
        // Since the timeline fix (frames carry their arrival timestamps, see
        // `localplay_encoder::ffmpeg`) media time and the wall clock advance together, so
        // "10s of footage" and "10 real seconds" now agree on a machine of any speed — the
        // earlier divergence, media at 0.81x, is what the ledger records as fixed. If the
        // buffer holds less than `pre_ms` of media at the trigger, `RingBuffer::trigger`
        // already warns and splices the truncated front — that path is unchanged.
        let trigger_ms = self.ledger.span_ms();
        // Wall-clock value, kept for telemetry only: nothing below reads it, because mixing
        // the two clocks is what made the post-roll unreachable. Logged next to the media
        // value so the two can be compared in a soak: `drift` is wall minus media and should
        // sit still (at the encoder's start-up offset plus the segment ffmpeg is still
        // appending to) rather than grow.
        let wall_ms = self.clock.ms_at(Instant::now());
        // The line describes the instant of the trigger, which is the same whether a
        // hotkey, a button or a game event asked for the clip; what the reason adds is who
        // asked. A manual clip logs exactly what it always logged.
        tracing::info!(
            "{}: media={trigger_ms}ms wall={wall_ms}ms (drift {}ms); waiting for post-roll",
            reason.describe(),
            wall_ms as i64 - trigger_ms as i64
        );

        // Wait for the post-roll to be written before splicing (spec §6.2 step 2).
        //
        // The budget is `post_ms` + a margin, never a constant: the trigger instant is "now"
        // on the media timeline and the wait covers the whole post-roll, which is `post_ms` of
        // media — real seconds, one for one, but still invisible to the ring until ffmpeg has
        // finished the segment carrying them. The margin absorbs that segment plus the
        // encoder's own lag (see `POST_ROLL_MARGIN`).
        let need_ms = trigger_ms + self.post_ms;
        let budget = Duration::from_millis(self.post_ms) + POST_ROLL_MARGIN;
        // One wait loop for both modes and for the microphone: which ledger the span comes
        // from, and whether a second input is drained alongside the game audio, are the only
        // two things that differ (see `pump_until_span_on`).
        let counts = pump_until_span_on(
            &mut self.pacer,
            self.ledger.as_ring(),
            self.capture.as_mut(),
            self.audio.as_mut(),
            self.mic.as_deref_mut(),
            self.encoder.as_mut(),
            need_ms,
            budget,
        )?;
        self.account(counts);

        let stem = format!("clip-{}", unix_seconds());
        let clip = self.ledger.trigger(trigger_ms, &stem)?;
        tracing::info!(
            "wrote {} ({}ms, {} bytes, encoder={})",
            clip.path.display(),
            clip.duration_ms,
            clip.size_bytes,
            clip.encoder
        );

        // Spec §6.2 step 6: index the clip that was just written. `trigger_ms` is
        // run-relative media time and the window starts `pre_ms` before it;
        // `ledger_origin_ms` moves that onto the ledger's timeline, which is the timeline
        // segment numbering — and so the footage itself — is measured on.
        let started_at_ms = self.ledger.origin_ms() + trigger_ms.saturating_sub(self.pre_ms);
        let id = index_clip(&self.store, &clip, started_at_ms);

        // Spec §5.5: an event-triggered clip says *why* it exists. The row's `at` is the
        // trigger instant (which is `pre_ms` into the clip, not its start), so the marker
        // lands on the moment that caused it. A failed insert is reported and not fatal, for
        // the same reason a failed clip insert is not: the footage is what the user asked
        // for, and the clip file is on disk either way.
        if let Some(event) = reason.event() {
            let at_ms = self.ledger.origin_ms() + trigger_ms;
            if let Err(err) = index_event(&self.store, event, at_ms, id) {
                tracing::error!(
                    "could not record the {} event that asked for this clip ({err:#}); the \
                     clip itself is kept, and the session timeline is missing one marker",
                    event.kind
                );
            }
        }

        self.status.clips.fetch_add(1, Ordering::Relaxed);
        Ok(RecordedClip { metadata: clip, id, started_at_ms })
    }

    /// Record an event that did not ask for a clip (see [`Recorder::note_event`]).
    fn note_event(&mut self, event: &GameEvent) -> Result<i64> {
        let at_ms = self.ledger.origin_ms() + self.ledger.span_ms();
        index_event(&self.store, event, at_ms, None)
    }

    /// Keep the `sessions` row's `size_bytes` current (spec §8.1, Phase 5).
    ///
    /// The retention cap counts `sessions.size_bytes`, so without this a multi-hour
    /// recording would be invisible to it until it stopped — and a session that is still
    /// recording is exactly the one nothing may evict (its scratch directory is footage
    /// that exists nowhere else). Runs on the tick that already scanned the ledger, so the
    /// number is the same one the status line prints and nothing extra is measured. A
    /// failure is reported once rather than five times a second: the tick is every
    /// [`STATUS_INTERVAL`].
    fn report_session_size(&mut self, bytes: u64) {
        if let Err(err) = self.store.set_session_size(self.session, bytes as i64) {
            if !self.size_warning_reported {
                tracing::warn!(
                    "could not record session #{}'s current size ({bytes} bytes) in the \
                     index: {err:#}. The sessions retention cap counts that number, so this \
                     recording is invisible to it until it stops.",
                    self.session
                );
                self.size_warning_reported = true;
            }
        }
    }

    /// Fold one pump's counts into the counters and publish them.
    fn account(&mut self, counts: PumpCounts) {
        self.frames += counts.submitted;
        self.skipped += counts.skipped;
        // Frames that reached ffmpeg: everything handed to the encoder, minus whatever its
        // bounded queue dropped because it could not keep up. This, not `frames=`, is "the
        // achieved rate" — it is the rate the recorded footage actually carries. The count
        // is cumulative, so only the newly-visible drops are taken out of the meter.
        let dropped = self.encoder.dropped_frames();
        let reached = counts.submitted.saturating_sub(dropped.saturating_sub(self.last_counted_dropped));
        self.last_counted_dropped = dropped;
        let now = Instant::now();
        self.achieved = self.rate.record(reached, now);
        self.status.publish_rates(
            self.frames,
            self.skipped,
            dropped,
            self.encoder.dropped_audio_blocks(),
            self.achieved,
            self.drift_ms(),
        );
        // The microphone's drop counter lives on the same status surface as the other two:
        // it is the same kind of fact (the encoder's bounded queue dropped a block), and a
        // voice track quietly losing blocks is exactly what a user needs to see.
        self.status.publish_mic(self.encoder.dropped_mic_audio_blocks());
    }

    /// Wall clock minus media time, in ms.
    fn drift_ms(&self) -> i64 {
        self.clock.ms_at(Instant::now()) as i64 - self.ledger.span_ms() as i64
    }

    /// Flush the encoder, close the sources, and close the session.
    ///
    /// Every step is attempted even if an earlier one failed: a shutdown that gave up
    /// halfway would leave the display captured or ffmpeg writing.
    fn shutdown(&mut self) -> Result<()> {
        // The encoder first: the segment it is still appending to has to be closed before
        // the ledger is scanned, or the last footage of the session is never indexed.
        let flushed = self.encoder.finish().context("flushing the encoder");
        let scanned = if flushed.is_ok() {
            self.ledger.scan().context("scanning for new segments at shutdown")
        } else {
            Ok(())
        };
        let ledger = match &self.ledger {
            Ledger::Buffer(ring) => ring.save_ledger().context("saving the ledger at shutdown"),
            // A session writes no ledger file: its segments are the directory listing, which
            // is exactly what makes them recoverable after a crash (see `crate::session`).
            Ledger::Session(_) => Ok(()),
        };
        // The sources are closed *before* the session is concatenated: a multi-gigabyte copy
        // must not keep the user's display under a Windows Graphics Capture session.
        let audio = self.audio.stop().context("stopping audio capture");
        let capture = self.capture.stop().context("stopping screen capture");
        let mic = match self.mic.take() {
            Some(mut mic) => mic.stop().context("stopping the microphone"),
            None => Ok(()),
        };
        let session = self.finish_session();
        flushed.and(scanned).and(ledger).and(audio).and(capture).and(mic).and(session)
    }

    /// Close this recording's `sessions` row — concatenating a full session first.
    ///
    /// * Buffer mode: the row is closed with the bytes the ring holds and **no file**
    ///   (spec §5.5's buffer-mode session: the ring evicts by its own cap and there is
    ///   nothing else to name).
    /// * Full-session mode: the segments are concatenated losslessly into
    ///   `sessions/session-<start>.mp4` (never re-encoded — see [`session::finalise`]), the
    ///   row names it with its real size, and the temporary segments are removed.
    ///
    /// # When the concatenation fails, and what is on disk afterwards
    ///
    /// A whole session needs transiently about **twice its own bytes** (the file is written
    /// in full before the segments go), so a volume that filled up during the recording
    /// cannot finish it. Nothing is lost to that, and nothing is hidden either:
    ///
    /// * every segment is kept — nothing is deleted on the way to a failure;
    /// * the partial output is removed, because it is not a session and it occupies the space
    ///   a retry needs;
    /// * the row is left **running** (`ended_at IS NULL`) on purpose: a running session is
    ///   never an eviction candidate, so the retention pass cannot delete the only copy of
    ///   footage that has no session file yet — and the next start's recovery pass
    ///   ([`session::recover_sessions`]) finishes the job once there is room;
    /// * the error is returned, so `stop()` — and a CLI's exit code — reports it.
    fn finish_session(&mut self) -> Result<()> {
        match self.mode {
            RecordingMode::ReplayBuffer => {
                let stats = self.ledger.stats();
                self.store
                    .end_session(self.session, now_ms(), None, stats.bytes_on_disk as i64)
                    .with_context(|| format!("closing session #{}", self.session))?;
                tracing::info!(
                    "buffer session #{} closed: {} segment(s), {} bytes in the scratch ring, \
                     no session file",
                    self.session,
                    stats.segments,
                    stats.bytes_on_disk
                );
                Ok(())
            }
            RecordingMode::FullSession => {
                // The encoder has been flushed and its child has exited, so the newest
                // segment file is complete and belongs in the session (the live scan
                // deliberately does not trust the newest file while ffmpeg is writing it).
                self.ledger.scan_all()?;
                let (segments, directory, bin) = match &self.ledger {
                    Ledger::Session(session) => (
                        session.segments().to_vec(),
                        session.dir().to_path_buf(),
                        session.bin().clone(),
                    ),
                    Ledger::Buffer(_) => bail!(
                        "a full-session recording has no session segment store: this is a \
                         bug in the recorder (mode and ledger disagree)"
                    ),
                };
                let out = session::session_file_path(&self.sessions_dir, self.session_started_ms);
                let bytes: u64 = segments.iter().map(|s| s.bytes).sum();
                if segments.is_empty() {
                    // A recording that never produced a segment — a start that failed after
                    // the row was opened, a game that started and stopped inside ffmpeg's
                    // start-up. Closed honestly, with no file to name.
                    self.store
                        .end_session(self.session, now_ms(), None, 0)
                        .with_context(|| format!("closing empty session #{}", self.session))?;
                    session::remove_session_dir(&directory);
                    tracing::info!(
                        "session #{} recorded no segments; closed with no file",
                        self.session
                    );
                    return Ok(());
                }

                let meta = session::finalise(
                    &bin,
                    &out,
                    &segments,
                    &self.encoder_name,
                    self.microphone,
                )
                .with_context(|| {
                    format!(
                        "finalising session #{} ({} segment(s), {bytes} bytes, in {})",
                        self.session,
                        segments.len(),
                        directory.display()
                    )
                })?;

                self.store
                    .end_session(
                        self.session,
                        now_ms(),
                        Some(&meta.path.display().to_string()),
                        meta.size_bytes as i64,
                    )
                    .with_context(|| format!("closing session #{}", self.session))?;
                let removed = session::remove_session_dir(&directory);
                tracing::info!(
                    "session #{} finalised: {} ({}ms, {} bytes) concatenated from {} \
                     segment(s); {removed} bytes of temporary segments removed",
                    self.session,
                    meta.path.display(),
                    meta.duration_ms,
                    meta.size_bytes,
                    segments.len()
                );
                Ok(())
            }
        }
    }

    /// Record a failure and let the loop end. The message is both logged and kept in the
    /// status, because a front-end that only polls `status()` must still be able to say
    /// what happened.
    fn fail(&mut self, err: anyhow::Error) {
        let message = format!("{err:#}");
        tracing::error!("the recorder stopped: {message}");
        *self.status.error.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(message);
    }
}

/// Resolve the encoder's codec and vendor — proving this machine can actually encode —
/// **before** any capture backend is created.
///
/// WHY THIS RUNS FIRST. On Windows, creating the video backend opens a Windows Graphics
/// Capture session on the primary monitor: it starts capturing the user's screen. The
/// vendor cannot be known to work until `select_vendor` has driven a one-frame smoke test
/// of the candidate encoder, and that smoke test is exactly where an
/// advertised-but-unusable vendor is caught (the measured `h264_amf` on a box with no AMD
/// driver, which otherwise died mid-capture with "video writer thread has stopped"). If
/// the smoke test ran *after* the backend was created, a machine whose encoder cannot
/// start would have had its screen captured before the process discovered it could encode
/// nothing. Resolving the vendor first turns that into a fast, actionable startup failure
/// that never touches the display. Nothing is lost by moving it earlier: the smoke test
/// encodes its own synthetic frame, so it needs no real capture size.
///
/// The size-dependent half of the encoder decision does still need the backend (the
/// rawvideo pipe is declared from the backend's `native_size`), so it stays in
/// [`build_encode_config`] and runs once the backend exists.
///
/// The returned vendor is `None` for `dev_software_encoder`, the libx264 smoke-test path,
/// which needs no GPU vendor at all and must not be gated behind a hardware check.
fn resolve_encoder(
    bin: &FfmpegBinaries,
    encode: &EncodeSection,
    dev_software: bool,
) -> Result<(VideoCodec, Option<Vendor>)> {
    let codec = parse_codec(&encode.codec)?;
    if dev_software {
        #[cfg(any(test, feature = "test-encoders"))]
        {
            return Ok((codec, None));
        }
        #[cfg(not(any(test, feature = "test-encoders")))]
        {
            bail!("--dev-software-encoder requires building with `--features test-encoders`");
        }
    }
    Ok((codec, Some(select_vendor(bin, &encode.vendor, codec)?)))
}

/// The rate decision, said out loud.
///
/// This line is a deliverable, not a debug aid. It is the difference between "capture is
/// running at 24fps" as an unexplained number and as a measured fact about *this* machine at
/// *this* resolution — and it is the number that decides how much of the capture work is
/// spent on frames the encoder will keep.
///
/// It is **not** the timeline guarantee, and the text below must not claim to be: the media
/// timeline is the frames' arrival timestamps and does not depend on this measurement at all
/// (see `localplay_encoder::throughput` for the 62-measured-vs-8-achieved measurement that
/// settled that). What adapting buys is the capture work of frames that would be dropped, and
/// a warning a user can act on before recording instead of after.
///
/// Nothing here is printed from the *configured* value when the effective one differs: a
/// reader must never have to work out which of the two is in force.
///
/// * **Reduced** (`WARN`) — the configured rate is not achievable here. It says what was
///   measured, what the pipeline therefore runs at, how a user who wants a higher rate can get
///   one (a smaller output size, or a lower fps), plus the escape hatch (`adapt_fps = false`)
///   and what it costs (dropped frames: moments of the recording held on the previous frame).
/// * **Not reduced** (`INFO`) — one short line, because there is nothing to warn about and a
///   user should still be able to see that the measurement happened and what it said.
/// * **Not measured at all** (`INFO`) — adaptation is off; say so, since the absence of a
///   measurement is itself a decision someone made.
fn log_rate_decision(decision: &FpsDecision, encoder: &str, native: (u32, u32)) {
    let Some(m) = decision.measurement() else {
        tracing::info!(
            "encode rate: not measured (encode.adapt_fps = false), so capture runs at the \
             configured {}fps at {}x{} whatever this machine can sustain: the encoder's queue \
             will drop any frame it cannot take, so the picture can hold frames the machine \
             could not encode. The media timeline is unaffected — it is the frames' arrival \
             timestamps",
            decision.configured(),
            native.0,
            native.1
        );
        return;
    };

    // The size is spelled out from the measurement rather than from the configuration: the
    // number is a property of the geometry it was taken at, and when the output is scaled
    // both sizes are part of what was measured (the frames fed *and* the frames encoded).
    let where_measured = if m.source_size == m.output_size {
        format!("at {}x{}", m.source_size.0, m.source_size.1)
    } else {
        format!(
            "at {}x{} frames scaled to a {}x{} output",
            m.source_size.0, m.source_size.1, m.output_size.0, m.output_size.1
        )
    };
    let measured = format!(
        "{:.1}fps sustainable {where_measured} with {encoder} ({} frames over {:.1}s)",
        m.fps,
        m.frames,
        m.window.as_secs_f64()
    );

    if decision.reduced() {
        tracing::warn!(
            "encode.fps = {} is not achievable at {}x{} on this machine: {measured}. Capturing \
             at {}fps instead — the same rate the encoder child is told — so that the capture \
             is not paying a readback and a copy for frames the encoder will throw away. The \
             media timeline does not depend on this rate: frames carry their arrival \
             timestamps, so a clip covers the seconds it was captured over either way. The \
             configured rate was not reached at this resolution: lower encode.output_size \
             (e.g. \"1920x1080\") or encode.fps to a rate this machine holds, or set \
             encode.adapt_fps = false to declare {}fps anyway and accept the frames that will \
             be dropped and held in the picture.",
            decision.configured(),
            native.0,
            native.1,
            decision.effective(),
            decision.configured()
        );
    } else {
        tracing::info!(
            "encode rate: {measured}; encode.fps = {} is within that, so capture runs at the \
             configured {}fps",
            decision.configured(),
            decision.effective()
        );
    }
}

/// The video codec the config asks for. Machine-independent, so it is resolved alongside
/// the vendor, before any capture backend exists.
pub fn parse_codec(spec: &str) -> Result<VideoCodec> {    match spec {
        "h264" => Ok(VideoCodec::H264),
        "hevc" => Ok(VideoCodec::Hevc),
        other => bail!("unsupported encode.codec: {other}"),
    }
}

/// `"1920x1080"`, or `None` for the empty string (documented as "native capture
/// resolution", config.example.toml / spec §10).
pub fn parse_output_size(spec: &str) -> Result<Option<(u32, u32)>> {
    if spec.trim().is_empty() {
        return Ok(None);
    }
    spec.split_once('x')
        .and_then(|(w, h)| Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?)))
        .map(Some)
        .with_context(|| {
            format!("encode.output_size must be \"WIDTHxHEIGHT\", e.g. \"1920x1080\", got {spec:?}")
        })
}

/// The capture and audio backends for the configured [`Sources`].
fn build_sources(
    sources: Sources,
    fps: u32,
) -> Result<(Box<dyn CaptureBackend>, Box<dyn AudioBackend>)> {
    match sources {
        // The backend is chosen per platform: real WGC/WASAPI capture on Windows, the
        // synthetic stubs elsewhere. On Windows a missing capture backend is a hard error
        // there is no stub fallback to hide it behind.
        Sources::Platform => {
            let stub = StubConfig { width: STUB_CAPTURE_SIZE.0, height: STUB_CAPTURE_SIZE.1, fps };
            Ok((
                default_video_backend(stub)?,
                default_audio_backend(AudioFormat::default())?,
            ))
        }
        // Explicitly the synthetic pair. Only a test (or a front-end that says so out
        // loud) asks for these; nothing about them touches the display or the speakers.
        Sources::Stub(cfg) => Ok((
            Box::new(StubCapture::new(cfg)),
            Box::new(StubAudio::new(AudioFormat::default())),
        )),
    }
}

/// Build the encoder's configuration from the already-resolved codec and vendor plus the
/// capture backend's own frame geometry — without starting the encoder. Hardware is the
/// only shipping path.
///
/// The vendor is resolved earlier, before the capture backend exists (see
/// [`resolve_encoder`]); `None` here is the `dev_software_encoder` (libx264) path.
/// `native_size` is the capture backend's own frame geometry: the monitor for WGC, the
/// configured size for the stub. The encoder's rawvideo pipe is declared from it, which is
/// why this size-dependent step has to run *after* the backend exists. Empty
/// `encode.output_size` means "encode at native resolution" (config.example.toml, spec
/// §10); any other value is the output size, scaled from the native frames.
///
/// Separate from [`spawn_encoder`] because the engine has to know the encoder's name (for
/// the ring's clip metadata) and, more importantly, has to decide the segment numbering
/// from the scratch directory *before* the child starts: the number is an ffmpeg argument,
/// so it cannot be changed afterwards.
fn build_encode_config(
    cfg: &RecorderConfig,
    scratch_dir: &Path,
    codec: VideoCodec,
    vendor: Option<Vendor>,
    native_size: (u32, u32),
) -> Result<EncodeConfig> {
    let output_size = parse_output_size(&cfg.encode.output_size)?.unwrap_or(native_size);
    let segment_ms = cfg.buffer.segment_time * 1000;

    let encode_cfg = match vendor {
        Some(vendor) => EncodeConfig::hardware(
            codec,
            vendor,
            native_size, // source: the frames the capture pipe delivers
            output_size, // output: scaled from the source when they differ
            cfg.encode.fps,
            cfg.encode.bitrate_kbps,
            segment_ms,
            scratch_dir.to_path_buf(),
        ),
        // `vendor` is `None` only for `dev_software_encoder`, which is only reachable from
        // a build that carries the software encoder (see [`resolve_encoder`]).
        None => {
            #[cfg(any(test, feature = "test-encoders"))]
            {
                // The message names the CLI's flag, which is the only way a user can
                // reach this path (there is no configuration setting for it, spec §3.2).
                // Kept verbatim so the move into this crate did not change the CLI's log.
                tracing::warn!(
                    "--dev-software-encoder: using libx264. This is for smoke-testing \
                     the pipeline only and is NOT a supported configuration."
                );
                let mut c = EncodeConfig::for_tests_software(
                    codec,
                    native_size.0,
                    native_size.1,
                    cfg.encode.fps,
                    scratch_dir.to_path_buf(),
                    segment_ms,
                );
                // The dev encoder scales the same way the hardware one does: frames
                // arrive at the native size, the output is `output_size`.
                c.output_size = output_size;
                c
            }
            #[cfg(not(any(test, feature = "test-encoders")))]
            {
                bail!("--dev-software-encoder requires building with `--features test-encoders`");
            }
        }
    };

    // The rawvideo pipe is declared from `source_size`; if it did not equal the backend's
    // native size ffmpeg would mis-read every frame. This derives it from `native_size`,
    // so this guards against a future edit breaking that link.
    if encode_cfg.source_size != native_size {
        bail!(
            "encoder source size {}x{} does not match the capture backend's native size \
             {}x{}; the raw video pipe would be mis-read",
            encode_cfg.source_size.0,
            encode_cfg.source_size.1,
            native_size.0,
            native_size.1
        );
    }

    Ok(encode_cfg)
}

/// Start the ffmpeg child for a configuration whose `start_number` has already been
/// decided (see [`build_encode_config`] and `RingBuffer::reserve_segment_number`).
fn spawn_encoder(
    bin: &FfmpegBinaries,
    encode_cfg: &EncodeConfig,
) -> Result<(Box<dyn Encoder>, String)> {
    let encoder = FfmpegEncoder::spawn(bin, encode_cfg)?;
    tracing::info!(
        "rawvideo -s={}x{} (capture native), encode output {}x{}{}",
        encode_cfg.source_size.0,
        encode_cfg.source_size.1,
        encode_cfg.output_size.0,
        encode_cfg.output_size.1,
        if encode_cfg.source_size == encode_cfg.output_size { "" } else { " (scaled)" }
    );
    let name = encoder.active_encoder().to_string();
    Ok((Box::new(encoder), name))
}
