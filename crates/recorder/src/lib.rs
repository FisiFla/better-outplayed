//! The recording engine: capture -> audio -> encode -> segment ring -> clip.
//!
//! Both front-ends (the CLI and the desktop shell) are drivers over [`Recorder`]. It owns the
//! capture and audio backends, the encoder, the ring (RAM for a replay buffer, files for a
//! session), the [`FramePacer`], the capture clock, the pump thread and the clip-index insertion.
//! It owns no hotkey and no UI, and reads no config file: front-ends hand it a [`RecorderConfig`].
//!
//! # Triggers
//!
//! A trigger carries a [`ClipReason`]. A manual clip records only the clip; a clip an integration
//! asked for also writes the reason into the `events` table, linked to the clip. Both go through
//! [`Recorder::clip_now_with`], so they share pre-roll, post-roll and media-time semantics. A
//! marker event ([`EventKind::is_highlight`] false) is recorded without a clip via
//! [`Recorder::note_event`].
//!
//! # Threads
//!
//! [`Recorder::start`] runs the whole startup handshake on the calling thread (index, storage
//! policy, encoder smoke test, capture backend, ring, ffmpeg) and returns its errors there, so an
//! unusable machine fails at startup. Only the pump loop runs on the spawned thread.
//! [`Recorder::clip_now`] hands the trigger to that thread because the trigger must pump the
//! encoder and may wait [`POST_ROLL_MARGIN`] plus the post-roll; [`Recorder::status`] only reads
//! atomics and never blocks the loop.
//!
//! # Modes, microphone, game watcher
//!
//! * [`RecordingMode`]: [`RecordingMode::ReplayBuffer`] (default) or [`RecordingMode::FullSession`],
//!   which writes a per-session directory exempt from the RAM cap and concatenates it into
//!   one `session-<timestamp>.mp4` at stop (see [`session`]). Both open a `sessions` row managed
//!   by retention.
//! * Microphone ([`MicSection`], off by default): a second audio input, so clips carry two audio
//!   tracks. If it cannot start, the recording fails at start rather than producing a silent voice track.
//! * Game watcher ([`replayed_events::process::GamesSection`], `[games] auto_record`, off by
//!   default): a recording starts and stops with a watched game. With it off no watcher thread
//!   exists at all.
//!
//! [`Recorder::start`] is [`Recorder::start_with_options`] with [`RecorderOptions::default`].
//! [`RecorderConfig`] has one defaulted field ([`RecorderConfig::audio_bitrate_kbps`]) because the
//! desktop shell constructs it field by field; other settings travel in [`RecorderOptions`].
//!
//! # The media timeline
//!
//! Everything a trigger, a marker or a clip's chapter is measured on is the *ledger's* clock,
//! never the wall clock's: the footage is measured on it, so a position on it names a moment of
//! the recording. The two clocks are close, and the engine logs the difference when a recording
//! starts, but they are not the same clock:
//!
//! * The RAM ring's clock is **measured**: a fragment's length comes from its own sample table,
//!   so the ring's position is the real media time.
//! * A session's clock is **nominal**: a segment is a whole number of seconds by construction, so
//!   the ledger's position runs ahead of the real media by up to one frame interval per segment
//!   (about 3% of the timeline at 30fps, 10% at 10fps), and it lags the wall clock by up to a
//!   whole segment because a segment counts only once a later one exists.
//!
//! So "30s of pre-roll" is 30s of *real* media in a replay buffer and 30s of *nominal* media in
//! a session, and the post-roll wait is sized for the difference (see [`POST_ROLL_MARGIN`] and
//! the engine's post-roll budget). The RAM path is the reference behaviour.
//!
//! # Invariants that must not regress
//!
//! * The trigger is expressed in the ring's media time (`span_ms`), never the wall clock, and the
//!   post-roll budget is `post_ms + margin`; a wall-clock trigger could ask for footage not yet written.
//! * A recording's index connection is given back on **every** failure, not only when a recording
//!   ends: without it one failed start would end auto-record for the life of the process.
//! * The clip's splice runs off the pump thread. On it, the pump stops for as long as ffmpeg
//!   takes, and the frames that follow carry a timestamp a whole splice later — a frozen picture
//!   in the recording, at every clip.
//! * An event is stamped with the media time of the moment it *arrived*, not of the moment its
//!   clip was cut: a burst is coalesced before the clip is made, and that wait can run for most
//!   of the pre-roll.
//! * A clip is dated by the frame it really starts on — the resolved window's start, not the
//!   requested one — because the index row and the chapter offsets are measured from that frame.
//! * The pacer decides before the frame is materialised, and non-due frames are drained with
//!   [`CaptureBackend::discard_pending`] so the GPU readback is skipped.
//! * The encoder is smoke-tested before any capture backend exists, so an unusable encoder never
//!   opens a capture session on the display. The microphone is built on the same side of that
//!   line (see `Prepared::begin`).
//! * Clips are spliced losslessly (`-c copy`) and the index row is written before the file could
//!   be evicted.
//! * Cleanup runs at startup and every [`CLEANUP_INTERVAL`], over clips and sessions independently
//!   ([`cleanup_pass`]).
//! * The status line (`frames= segments= bytes= span= evicted= dropped= dropped_audio=
//!   dropped_mic= skipped= fps=`) stays observable, and counters stay readable from other threads.
//! * One writer thread per input: the pump submits video, game audio and microphone into the
//!   encoder's per-input queues, never writing two inputs synchronously
//!   ([`pump_once_counted_with_mic`]).

// Test bodies may unwrap; production code may not. Scoped to the test build of this crate, so
// it cannot hide an unwrap in shipped code.
#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod config;
pub mod config_schema;
pub mod event_driver;
pub mod fps;
pub mod index;
pub mod memory_ring;
pub mod pump;
pub mod session;

#[cfg(test)]
mod tests;

pub use config::{
    BufferSection, CaptureSection, EncodeSection, MicSection, RecorderSection, RecordingMode,
    SessionStorageRules, StorageSection,
};
pub use event_driver::{GsiBinding, GsiGameInfo, GsiHandle};
pub use fps::FpsDecision;
pub use index::{
    cleanup_pass, clip_chapters, index_bookmark, index_clip, index_event, now_ms, open_clip_index,
    unix_millis, CleanupReport, BOOKMARK_KIND,
};
pub use pump::{
    guard_frame_size, pump_once, pump_once_counted, pump_once_counted_with_mic, pump_until_span_on,
    FramePacer, MediaRing, PumpCounts, RateMeter, FRAME_POLL, PACER_RESYNC_AFTER_INTERVALS,
    POST_ROLL_MARGIN, POST_ROLL_SCAN_INTERVAL, RATE_WINDOW,
};

use anyhow::{bail, Context, Result};
use memory_ring::MemoryRing;
use replayed_capture::platform::{default_video_backend, game_audio_backend};
use replayed_capture::stub::{StubAudio, StubCapture, StubConfig};
use replayed_capture::wasapi_mic::{StubMicrophone, MICROPHONE_FORMAT};
use replayed_capture::{AudioBackend, AudioFormat, CaptureBackend};
use replayed_encoder::probe::select_vendor;
use replayed_encoder::{
    EncodeConfig, Encoder, FfmpegEncoder, MicAudioSpec, ThroughputMeasurement, Vendor, VideoCodec,
};
use replayed_events::process::{GamesSection, PresenceChange, WatchHandle};
use replayed_events::{CaptureClock, GameEvent};
use replayed_media::{edit, FfmpegBinaries};
use replayed_replay::buffer::{BufferConfig, BufferStats};
use replayed_replay::splice::{ClipMetadata, ClipSplicer};
use replayed_store::{Event, Store};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The frame size the synthetic capture backend produces off Windows.
///
/// Deliberately not 1920x1080, so the rawvideo pipe is visibly sized from the backend.
pub const STUB_CAPTURE_SIZE: (u32, u32) = (1280, 720);

/// How often the scratch ring is scanned, the status published, and the status line emitted.
///
/// One tick for all three: the counters only move when a segment finalises.
pub const STATUS_INTERVAL: Duration = Duration::from_millis(200);

/// At most one "the encoder cannot sustain the configured fps" warning per this long.
///
/// A machine in that state stays there all run; the `debug` status line is the continuous record.
pub const DROP_WARN_INTERVAL: Duration = Duration::from_secs(10);

/// How often the storage policy is re-applied to the clips directory while running.
///
/// Clips only accumulate when triggered and a pass is cheap (a `list_clips` plus a `SUM`), so five
/// minutes keeps a session's clips from drifting far past `storage.max_total_bytes`. Startup also applies it.
pub const CLEANUP_INTERVAL: Duration = Duration::from_secs(300);

/// Where the frames come from.
///
/// [`Sources::Platform`] is the shipping path (WGC and WASAPI loopback on Windows; stubs elsewhere,
/// and `replayed_capture::platform` refuses a stub fallback on Windows). [`Sources::Stub`] selects
/// the synthetic sources explicitly for tests and is never reachable from a config file.
#[derive(Debug, Clone, Copy)]
pub enum Sources {
    Platform,
    Stub(StubConfig),
}

/// Everything [`Recorder::start`] needs, resolved by the front-end.
///
/// Sections are `config.toml`'s verbatim; the recorder applies the "empty means default" path rule
/// itself so both front-ends resolve `scratch/`, `clips/` and `replayed.db` identically.
#[derive(Debug, Clone)]
pub struct RecorderConfig {
    /// Where ffmpeg and ffprobe live, discovered by the front-end.
    pub bin: FfmpegBinaries,
    /// The application data directory; `scratch/`, `clips/` and `replayed.db` live here unless overridden.
    pub app_data_dir: PathBuf,
    pub buffer: BufferSection,
    pub encode: EncodeSection,
    pub storage: StorageSection,
    /// `[capture]` — which monitor or window `Sources::Platform` records.
    pub capture: CaptureSection,
    /// `[audio] bitrate_kbps`, as the CLI read it.
    ///
    /// Lives here, not in [`EncodeSection`], so `[audio]` stays that setting's only home. `None` (the
    /// desktop shell does not parse `[audio]`) resolves to
    /// [`replayed_encoder::DEFAULT_AUDIO_BITRATE_KBPS`] in [`build_encode_config`].
    pub audio_bitrate_kbps: Option<u32>,
    /// `[audio] enabled`: whether this recording carries the game-audio track.
    ///
    /// `None` (the desktop shell) means `true`. `Some(false)` is video-only: no loopback backend, no
    /// audio input, no audio stream. Read it through [`RecorderConfig::game_audio`].
    pub audio_enabled: Option<bool>,
    pub sources: Sources,
    /// Encode with libx264 (the CLI's `--dev-software-encoder`) for smoke tests on a host with no GPU
    /// encoder. Only reachable from a build with `test-encoders`, never from a config file.
    pub dev_software_encoder: bool,
}

impl RecorderConfig {
    /// The scratch directory this configuration records into.
    pub fn scratch_dir(&self) -> PathBuf {
        resolve_dir(&self.app_data_dir, "scratch", &self.buffer.scratch_dir)
    }

    /// The clips directory this configuration writes into (the same rule as the desktop `AppPaths`).
    pub fn clips_dir(&self) -> PathBuf {
        resolve_dir(&self.app_data_dir, "clips", &self.storage.clips_dir)
    }

    /// The sessions area: a full session's segment directory and its concatenated file.
    ///
    /// Its own directory, not a corner of `scratch/`: nothing drops segments out of it while recording.
    pub fn sessions_dir(&self) -> PathBuf {
        resolve_dir(
            &self.app_data_dir,
            "sessions",
            &self.storage.sessions.sessions_dir,
        )
    }

    /// The clip index both this engine and the desktop shell open.
    pub fn db_path(&self) -> PathBuf {
        self.app_data_dir.join("replayed.db")
    }
}

/// An empty setting means "under the application data directory".
fn resolve_dir(app_data_dir: &Path, default_name: &str, setting: &str) -> PathBuf {
    if setting.is_empty() {
        app_data_dir.join(default_name)
    } else {
        PathBuf::from(setting)
    }
}

/// Everything [`Recorder::start_with_options`] needs that is not in the recording sections.
///
/// Its own struct (not more `RecorderConfig` fields) because the desktop shell builds that field
/// by field and a new required field would break it. [`Recorder::start`] passes the `Default`.
#[derive(Debug, Clone, Default)]
pub struct RecorderOptions {
    /// What to record: the rolling buffer, or the whole session (see [`RecordingMode`]).
    pub mode: RecordingMode,
    /// In replay-buffer mode, also keep the whole recording as a session file ([`RecorderSection::save_session`]).
    pub save_session: bool,
    /// The optional microphone track ([`MicSection`]; off by default).
    pub mic: MicSection,
    /// `[games]`: whether a watched game starting is itself a reason to record, and which titles count.
    pub games: GamesSection,
    /// The game a driver detected, written into the `sessions` row; `None` for a person-started recording.
    pub game: Option<String>,
    /// A test hook: the recording starts that must fail (see [`FailStarts`]).
    ///
    /// The field does not exist outside this crate's own tests, so a front-end cannot set it.
    #[cfg(test)]
    pub fail_starts: FailStarts,
}

/// A test hook: how many of the next recording starts fail, and why.
///
/// Production has none: a start is refused only by a real failure. The hook exists because the
/// rule it guards — a failed start must not take the recorder's index connection with it — is
/// invisible otherwise: a start that fails *before* it takes the store proves nothing.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct FailStarts(Arc<Mutex<usize>>);

#[cfg(test)]
impl FailStarts {
    /// The next `count` starts fail.
    pub fn next(count: usize) -> Self {
        Self(Arc::new(Mutex::new(count)))
    }

    /// Whether this start must fail, and the reason it does.
    fn fail(&self) -> Option<&'static str> {
        let mut remaining = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *remaining == 0 {
            return None;
        }
        *remaining -= 1;
        Some("the test made this start fail")
    }
}

// `Default` is the replay buffer, no microphone, no watcher, no game.

/// What the recording engine is doing right now.
///
/// Read from other threads: atomic loads plus a short-lived mutex for `error`, so a read never
/// waits on the capture loop and vice versa.
#[derive(Debug, Clone, PartialEq)]
pub struct RecorderStatus {
    /// `false` once `stop()` has returned or the loop has failed.
    pub running: bool,
    /// Video frames submitted to the encoder (the status line's `frames=`).
    pub frames: u64,
    /// Completed segments in the ring.
    pub segments: u64,
    /// Bytes the ring is holding: in RAM for a replay buffer, on disk for a full session (show `mode` beside it).
    pub bytes: u64,
    /// Media time the ring can prove it holds, in ms; the trigger position and the post-roll clock.
    pub span_ms: u64,
    /// Encoder video frames dropped because its queue was full.
    pub dropped: u64,
    /// The same for audio blocks.
    pub dropped_audio: u64,
    /// The same for microphone blocks; zero when there is no microphone track.
    pub dropped_mic_audio: u64,
    /// Whether this recording carries a microphone track at all (`[mic] enabled`).
    pub mic: bool,
    /// The loopback port of the encoder's microphone input; `None` means no second input exists.
    pub mic_port: Option<u16>,
    /// The mode this recorder was started in ([`RecordingMode`]).
    pub mode: RecordingMode,
    /// The game being recorded when a watched game triggered it; `None` otherwise.
    pub game: Option<String>,
    /// Whether a game watcher is armed (`[games] auto_record` was on at start). While `true` with
    /// `running` false, the recorder is armed and waiting for a game.
    pub watching_games: bool,
    /// Frames the backend offered that the pacer skipped *without* reading them back.
    pub skipped: u64,
    /// The achieved frame rate over the last [`RATE_WINDOW`]; 0.0 before one has closed.
    pub fps: f64,
    /// What `encode.fps` asked for, so the two can be shown side by side.
    pub configured_fps: u32,
    /// The rate the pipeline actually runs at (the pacer's rate and the encoder's `-framerate`).
    /// Below `configured_fps` when the startup probe found the machine cannot hold the configured
    /// rate and adaptation is on. Compare an achieved-rate readout against this.
    pub effective_fps: u32,
    /// Wall clock minus media time, in ms. Positive means the provable footage is behind the wall
    /// clock (only finished segments count, so this includes encoder lag). Expected to stay near
    /// its start-up offset (ffmpeg start-up plus one segment) on a healthy machine.
    pub drift_ms: i64,
    /// Clips this session has written.
    pub clips: u64,
    /// Set when the loop stopped because of a failure, so a front-end can say why.
    pub error: Option<String>,
    /// The encoder the most recent recording ran (`h264_nvenc`, `libx264`, ...); `None` until one
    /// has started. Outlives the recording.
    pub encoder: Option<String>,
}

impl RecorderStatus {
    /// The status of an engine that has never been started; `configured_fps` is 0.
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
            encoder: None,
        }
    }
}

/// Why a clip was taken; the seam through which game integrations enter.
///
/// A driver hands the engine a derived [`GameEvent`] and the clip goes through the same path as a
/// hotkey press: one trigger, one window, one splice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipReason {
    /// A hotkey press or a button. No `events` row is written.
    Manual,
    /// A game event derived by an integration; written to `events` and linked to the clip it produced.
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

/// How much footage one clip takes, when it is not the configured `[buffer]` window.
///
/// `None` fields mean the configured `pre_seconds` / `post_seconds`. Requests are clamped to the
/// buffer's ceiling (`max_pre_seconds` / `max_post_seconds`) rather than refused: a slightly
/// shorter clip beats none when the key was pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClipWindow {
    pub pre_seconds: Option<u64>,
    pub post_seconds: Option<u64>,
}

impl ClipWindow {
    pub const fn new(pre_seconds: u64, post_seconds: u64) -> Self {
        Self {
            pre_seconds: Some(pre_seconds),
            post_seconds: Some(post_seconds),
        }
    }

    /// The effective `(pre_ms, post_ms)`: the request (or `default_*`) clamped to `max_*`. An empty
    /// window falls back to one second of post-roll, because a clip needs at least one frame.
    pub fn resolve(
        self,
        default_pre_ms: u64,
        default_post_ms: u64,
        max_pre_ms: u64,
        max_post_ms: u64,
    ) -> (u64, u64) {
        let pre = self
            .pre_seconds
            .map_or(default_pre_ms, |s| s.saturating_mul(1000))
            .min(max_pre_ms);
        let post = self
            .post_seconds
            .map_or(default_post_ms, |s| s.saturating_mul(1000))
            .min(max_post_ms);
        if pre + post == 0 {
            (0, 1000)
        } else {
            (pre, post)
        }
    }
}

/// What a trigger produced.
#[derive(Debug, Clone)]
pub struct RecordedClip {
    /// The file that was written, with what the splice measured it at.
    pub metadata: ClipMetadata,
    /// The `clips` row id when the index write succeeded. `None` means the file is on disk
    /// unindexed, which [`index_clip`] logs at ERROR.
    pub id: Option<i64>,
    /// The clip's first frame on the ring's media timeline (the index row's `started_at`).
    pub started_at_ms: u64,
}

/// A running recording session. [`Recorder::start`] returns one with the pump loop already
/// running on its own thread; dropping it stops that thread (see [`Recorder::stop`]).
pub struct Recorder {
    /// Commands to the recording running now, if any.
    ///
    /// A game-driven recorder replaces this on every start and clears it on every stop, so
    /// [`Recorder::clip_now`] errors clearly when none exists. Dropping the last sender disconnects
    /// the loop, which is how a dropped `Recorder` stops capture without `stop()`.
    commands: Arc<Mutex<Option<Sender<Command>>>>,
    /// The engine thread of a recording this `Recorder` started itself. Games mode leaves this
    /// `None`: the supervisor owns the thread ([`Recorder::stop`] signals the supervisor instead).
    ///
    /// The handle yields the store the thread was given, so a game-driven recorder can hand it
    /// to the next recording; `None` means the thread ended without one (see [`Prepared::begin`]).
    thread: Mutex<Option<JoinHandle<Option<Store>>>>,
    /// The game-presence supervisor, if a front-end asked for one. `None` is what guarantees that
    /// with `auto_record = false` nothing is watched.
    supervisor: Mutex<Option<Supervisor>>,
    status: Arc<SharedStatus>,
    /// The mode this recorder was started in (reported by [`Recorder::status`]).
    mode: RecordingMode,
    /// Whether this recorder was started with a microphone track ([`MicSection::enabled`]).
    mic: bool,
    /// How long a clip request may take before the caller gives up.
    ///
    /// The post-roll wait and the splice are both bounded, and the bound is what lets
    /// [`Recorder::clip_now_in`] give up rather than wait for ever: the index writes that
    /// follow them are not bounded at all, and a locked database or a dead mount would
    /// otherwise hang the caller for good.
    clip_budget: Duration,
}

/// A request from a caller on another thread to the pump loop.
enum Command {
    /// Take a clip now, for this reason. The reply is the clip, or why there is none.
    Clip {
        reason: ClipReason,
        window: ClipWindow,
        /// When the event happened, on the media timeline, when the caller knew it.
        ///
        /// A burst of events is coalesced before the clip is cut, and the coalescing can run
        /// for most of the pre-roll: the request instant is then long after the moment the
        /// event describes, and a marker stamped with it points at the wrong second of the
        /// recording. `None` means "now", which is right for a hotkey and wrong for an event.
        event_at_ms: Option<u64>,
        reply: Sender<Result<RecordedClip>>,
    },
    /// Record an event that did not ask for a clip (a game or round boundary). The reply is
    /// the `events` row's id, so a caller can tell that it landed.
    Note {
        event: GameEvent,
        /// When the event happened, on the media timeline, when the caller knew it. `None`
        /// means "now": the same reason as [`Command::Clip::event_at_ms`].
        at_ms: Option<u64>,
        reply: Sender<Result<i64>>,
    },
    /// Flush the encoder and end the loop.
    Stop,
}

/// The counters the loop publishes for [`Recorder::status`].
///
/// The fields are independent and none gates a memory access, so `Relaxed` is sufficient and no
/// reader can block the capture loop.
///
/// The two lifecycle flags are the exception: `running` and `watching` are written once per
/// recording and read by other threads, and they are read and written with `SeqCst` so a reader
/// that asks "is it running?" never sees a stale answer after a stop. They gate nothing either —
/// a reader that sees a stale `running` still reads the counters, which are independent.
#[derive(Debug)]
struct SharedStatus {
    running: AtomicBool,
    frames: AtomicU64,
    segments: AtomicU64,
    bytes: AtomicU64,
    span_ms: AtomicU64,
    /// The ledger's position on the media timeline, in ms: where the recording stands *now*.
    ///
    /// Published on every pump so a caller on another thread can stamp an event with the media
    /// time of the moment it happened. The engine's own clock is the ledger's, and the ledger's
    /// span only moves when a segment is finished, so this is up to one segment behind the wall
    /// clock — the same clock the footage is measured on, which is what a marker must use.
    media_ms: AtomicU64,
    dropped: AtomicU64,
    dropped_audio: AtomicU64,
    skipped: AtomicU64,
    clips: AtomicU64,
    drift_ms: AtomicI64,
    /// The achieved rate, as `f64::to_bits` (there is no `AtomicF64`).
    fps: AtomicU64,
    /// The rates the encoder was told and the configuration asked for, published per recording so a
    /// watcher-started recorder reports the running recording's numbers.
    configured_fps: AtomicU64,
    effective_fps: AtomicU64,
    /// Microphone blocks the encoder's queue dropped (`Encoder::dropped_mic_audio_blocks`).
    dropped_mic: AtomicU64,
    /// The encoder's microphone loopback port; 0 means no microphone input (a port is never 0).
    mic_port: AtomicU64,
    /// Whether a game watcher is armed (set while the supervisor thread lives).
    watching: AtomicBool,
    /// The game being recorded, if a watcher started this recording.
    game: Mutex<Option<String>>,
    /// The failure the loop stopped for, if any. A mutex since it is a string written at most once per run.
    error: Mutex<Option<String>>,
    /// The encoder the latest recording ran; kept after the recording ends.
    encoder: Mutex<Option<String>>,
}

impl SharedStatus {
    fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            segments: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            span_ms: AtomicU64::new(0),
            media_ms: AtomicU64::new(0),
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
            encoder: Mutex::new(None),
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

    /// The microphone's own drop counter; only meaningful when the recording has a microphone
    /// ([`RecorderStatus::mic`]).
    fn publish_mic(&self, dropped_mic: u64) {
        self.dropped_mic.store(dropped_mic, Ordering::Relaxed);
    }

    fn publish_ring(&self, segments: u64, bytes: u64, span_ms: u64) {
        self.segments.store(segments, Ordering::Relaxed);
        self.bytes.store(bytes, Ordering::Relaxed);
        self.span_ms.store(span_ms, Ordering::Relaxed);
    }

    /// Publish the per-recording numbers: the two rates and the microphone's port.
    fn publish_recording(&self, configured_fps: u32, effective_fps: u32, mic_port: Option<u16>) {
        self.configured_fps
            .store(u64::from(configured_fps), Ordering::Relaxed);
        self.effective_fps
            .store(u64::from(effective_fps), Ordering::Relaxed);
        self.mic_port
            .store(u64::from(mic_port.unwrap_or(0)), Ordering::Relaxed);
        self.dropped_mic.store(0, Ordering::Relaxed);
    }

    fn set_game(&self, game: Option<String>) {
        *self
            .game
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = game;
    }

    fn game(&self) -> Option<String> {
        self.game
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Recorder {
    /// Start recording with a replay buffer, no microphone, nothing watched: exactly
    /// [`Recorder::start_with_options`] with [`RecorderOptions::default`].
    ///
    /// The startup handshake runs on this thread, and the order is the point:
    ///
    /// 1. open (and migrate) the clip index, the one step that fails hard;
    /// 2. apply the storage policy and recover what a crash left behind (`session::recover_sessions`);
    /// 3. resolve the encoder's vendor with a one-frame smoke test, before any capture backend
    ///    exists, so an unusable encoder never opens a session on the display;
    /// 4. create the capture backend and read its native size; when a microphone was asked for,
    ///    create and start it before that, so one that cannot start fails the recording;
    /// 5. build the encoder config from that size, measure what the machine can sustain and decide
    ///    the pipeline rate ([`FpsDecision`]), start the ring, adopt what is on disk, pick the
    ///    first segment number, spawn ffmpeg and open the `sessions` row;
    /// 6. start capture and audio;
    /// 7. spawn the pump loop.
    ///
    /// Nothing is captured before step 6, so segment numbering cannot miss footage, and the probe
    /// runs before the capture session exists, so a probe failure costs only a startup error.
    pub fn start(cfg: RecorderConfig) -> Result<Recorder> {
        Self::start_with_options(cfg, RecorderOptions::default())
    }

    /// [`Recorder::start`] with the recording mode, the microphone and the game watcher.
    ///
    /// With `auto_record` off (the default, and what [`Recorder::start`] passes) it begins recording
    /// immediately. With it on it records nothing yet: it runs steps 1-3 above, starts the watcher and
    /// returns an armed recorder (`running` false, `watching_games` true). The rest of the handshake
    /// runs when a watched game starts, because creating the video backend starts screen capture,
    /// which must not happen to someone who is not playing.
    ///
    /// Each game session is recorded on its own: a `sessions` row opens when the game starts and closes
    /// when it stops (a full session is concatenated then), and retention runs between game sessions on
    /// the supervisor's thread.
    pub fn start_with_options(cfg: RecorderConfig, opts: RecorderOptions) -> Result<Recorder> {
        // The shipping measurement; binaries are cloned so the probe does not borrow `cfg` while it moves into `start_inner`.
        let bin = cfg.bin.clone();
        let measure = move |encode_cfg: &EncodeConfig| {
            replayed_encoder::throughput::measure_sustainable_fps(
                &bin,
                encode_cfg,
                replayed_encoder::throughput::PROBE_BUDGET,
            )
        };
        if !opts.games.watching() {
            // Nothing is watched, so nothing here creates a channel, a receiver or a thread:
            // `auto_record = false` is structural, not a flag a watcher checks.
            return Self::start_inner(cfg, opts, &measure, None);
        }
        // `GamesSection::start` returns `Ok(None)` when `auto_record` is off; handle the `Option` so the two cannot disagree silently.
        let (sink, changes) = mpsc::channel();
        let watch = opts.games.start(sink)?;
        Self::start_inner(cfg, opts, &measure, Some((changes, watch)))
    }

    /// [`Recorder::start`] with the throughput measurement injected (a `cfg(test)` hook).
    ///
    /// Lets a test hand in a measurement and assert what the pipeline did with it; production passes
    /// [`replayed_encoder::throughput::measure_sustainable_fps`]. The injected measurement applies
    /// only to the recording this call starts; a game-driven recorder measures with the shipping probe.
    ///
    /// # The one rate
    ///
    /// [`FpsDecision::effective`] is written into `encode_cfg.fps` once, and both consumers read that
    /// field: [`spawn_encoder`] (its `-framerate`, via `replayed_encoder::video_input_args`, the
    /// same function the probe used) and the [`FramePacer`]. The pacer therefore admits exactly what
    /// the encoder was told; computing the two separately is how the declared rate once diverged from
    /// the delivered one. `tests::the_pacer_and_the_encoder_are_told_the_same_rate` guards it.
    #[cfg(test)]
    fn start_with_measure(
        cfg: RecorderConfig,
        measure: &dyn Fn(&EncodeConfig) -> Result<ThroughputMeasurement>,
    ) -> Result<Recorder> {
        Self::start_inner(cfg, RecorderOptions::default(), measure, None)
    }

    /// [`Recorder::start_with_options`]'s body, with the presence channel injected.
    ///
    /// `presence` is `Some` when a game watcher drives this recorder: the watcher's changes plus its
    /// handle, kept alive as long as the supervisor runs. `None` means "record now".
    fn start_inner(
        cfg: RecorderConfig,
        opts: RecorderOptions,
        measure: &dyn Fn(&EncodeConfig) -> Result<ThroughputMeasurement>,
        presence: Option<(Receiver<PresenceChange>, Option<WatchHandle>)>,
    ) -> Result<Recorder> {
        // Resolved once; the session directory is derived from the sessions area (see `Prepared::begin`).
        let scratch_dir = cfg.scratch_dir();
        let clips_dir = cfg.clips_dir();
        let sessions_dir = cfg.sessions_dir();

        // 1. The index, the one step that fails hard (see `open_clip_index`).
        let store = open_clip_index(&cfg.db_path())?;

        // 2. The storage policy, once, over clips and sessions, so a previous run's leftovers are managed now.
        let mut cleanup = CleanupReport::default();
        cleanup_pass(&store, &cfg.storage, &clips_dir, &mut cleanup);

        // 2b. Crash recovery, before this run creates anything recovery could confuse for its own.
        let recovery = session::recover_sessions(
            &store,
            &sessions_dir,
            &cfg.bin,
            cfg.buffer.segment_time.saturating_mul(1000),
            &cfg.encode.codec,
        );
        log_recovery(&recovery);

        // 3. Resolve and smoke-test the encoder before any capture backend exists (see `resolve_encoder`).
        // In games mode this is the whole eager check.
        let (codec, vendor) = resolve_encoder(&cfg.bin, &cfg.encode, cfg.dev_software_encoder)?;

        let status = Arc::new(SharedStatus::new());
        let mode = opts.mode;
        let mic = opts.mic.enabled;
        let game = opts.game.clone();
        let clip_budget = clip_budget(&cfg);
        #[cfg(test)]
        let fail_starts = opts.fail_starts.clone();
        let prepared = Prepared {
            cfg,
            opts,
            codec,
            vendor,
            store: Some(store),
            scratch_dir,
            clips_dir,
            sessions_dir,
            #[cfg(test)]
            fail_starts,
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
                    clip_budget,
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
                    clip_budget,
                })
            }
        }
    }

    /// What the engine is doing right now. Never blocks the recording loop.
    pub fn status(&self) -> RecorderStatus {
        let status = &self.status;
        let error = status
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        RecorderStatus {
            running: status.running.load(Ordering::SeqCst),
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
            watching_games: status.watching.load(Ordering::SeqCst),
            skipped: status.skipped.load(Ordering::Relaxed),
            fps: f64::from_bits(status.fps.load(Ordering::Relaxed)),
            configured_fps: status.configured_fps.load(Ordering::Relaxed) as u32,
            effective_fps: status.effective_fps.load(Ordering::Relaxed) as u32,
            drift_ms: status.drift_ms.load(Ordering::Relaxed),
            clips: status.clips.load(Ordering::Relaxed),
            error,
            encoder: status
                .encoder
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
        }
    }

    /// Take a clip now: wait for the post-roll, splice it losslessly, index it.
    ///
    /// The trigger instant is the ring's media time, taken inside the loop when the trigger is
    /// handled, not a wall-clock instant sampled here: the ring only counts finished segments, so a
    /// wall-clock trigger can name footage the encoder has not written yet and time out.
    ///
    /// Blocks until the clip is written (the post-roll plus a margin); run it off a UI event loop.
    /// Equivalent to [`Recorder::clip_now_with`] with [`ClipReason::Manual`].
    pub fn clip_now(&self) -> Result<RecordedClip> {
        self.clip_now_with(ClipReason::Manual)
    }

    /// The same trigger, with the reason recorded.
    ///
    /// The single entry point for manual and automatic clipping, so a game event inherits the
    /// media-time trigger, post-roll wait and lossless splice. The reason adds one `events` row,
    /// written after the clip is indexed and linked to it.
    pub fn clip_now_with(&self, reason: ClipReason) -> Result<RecordedClip> {
        self.clip_now_in(reason, ClipWindow::default())
    }

    /// [`Recorder::clip_now_with`] with this clip's own [`ClipWindow`] (short/long hotkeys, per-game lengths).
    pub fn clip_now_in(&self, reason: ClipReason, window: ClipWindow) -> Result<RecordedClip> {
        self.clip_now_in_at(reason, window, None)
    }

    /// [`Recorder::clip_now_in`] with the moment the trigger happened, when the caller knows it.
    ///
    /// `event_at_ms` is on the recorder's media timeline (see [`Recorder::media_now_ms`]), and it
    /// is what the clip's marker and its chapter are stamped with. A burst of game events is
    /// coalesced before the clip is cut, and the coalescing can run for most of the pre-roll:
    /// without this the marker points at the request instant, which is up to the whole
    /// coalescing window later than the moment it describes.
    pub fn clip_now_in_at(
        &self,
        reason: ClipReason,
        window: ClipWindow,
        event_at_ms: Option<u64>,
    ) -> Result<RecordedClip> {
        let (reply, answer) = mpsc::channel();
        self.command_sender()?
            .send(Command::Clip {
                reason,
                window,
                event_at_ms,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("the recorder stopped before it could take the clip"))?;
        // A deadline, not an open wait. The post-roll and the splice are bounded, but the
        // index writes that follow them are not: a SQLite write on a locked file, a filesystem
        // call on a network drive, a `canonicalize` on a dead mount. A caller that waits for
        // ever on a recorder that has stopped answering is a caller that never returns.
        match answer.recv_timeout(self.clip_budget) {
            Ok(result) => result
                .map_err(|_| anyhow::anyhow!("the recorder stopped before it could take the clip")),
            Err(RecvTimeoutError::Timeout) => Err(anyhow::anyhow!(
                "the recorder did not answer the clip request within {}ms: it is busy, or the \
                 write that follows the clip is stuck",
                self.clip_budget.as_millis()
            )),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!(
                "the recorder stopped before it could take the clip"
            )),
        }
    }

    /// Record a derived event that did not ask for a clip, and return its `events` row id.
    ///
    /// Markers (a game or round boundary, see [`EventKind::is_highlight`]) belong on the session
    /// timeline but are not worth a clip. `at` is the trigger's position on the ring's media timeline,
    /// the same clock as a clip's `started_at`; `clip_id` is NULL. Blocking but cheap: one INSERT.
    pub fn note_event(&self, event: GameEvent) -> Result<i64> {
        self.note_event_at(event, None)
    }

    /// [`Recorder::note_event`] with the moment the event happened, on the media timeline.
    ///
    /// `at_ms` is what the row is stamped with, so a marker names the second of the recording
    /// it belongs to and not the second the request was made. `None` means "now", which is
    /// right for a caller that has no earlier knowledge and wrong for a coalesced burst.
    pub fn note_event_at(&self, event: GameEvent, at_ms: Option<u64>) -> Result<i64> {
        let (reply, answer) = mpsc::channel();
        self.command_sender()?
            .send(Command::Note {
                event,
                at_ms,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("the recorder stopped before it could note the event"))?;
        match answer.recv_timeout(self.clip_budget) {
            Ok(result) => result.map_err(|_| {
                anyhow::anyhow!("the recorder stopped before it could note the event")
            }),
            Err(RecvTimeoutError::Timeout) => Err(anyhow::anyhow!(
                "the recorder did not answer the event request within {}ms: the index write is \
                 stuck",
                self.clip_budget.as_millis()
            )),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!(
                "the recorder stopped before it could note the event"
            )),
        }
    }

    /// The recording's position on the media timeline, in ms.
    ///
    /// The clock the footage is measured on — the ledger's, not the wall clock's — so a caller
    /// that stamps an event with it puts the event where it happened on the recording's own
    /// timeline, next to the clips and the other events.
    ///
    /// It is the position of the last pump, so it trails the moment of the call by up to one
    /// pump interval; in session mode it also trails by up to a segment, because that ledger
    /// counts only finished segments (see [`session`]).
    pub fn media_now_ms(&self) -> u64 {
        self.status.media_ms.load(Ordering::Relaxed)
    }

    /// The command channel of the recording running now, or the error when there is none.
    ///
    /// Stopped and armed-and-waiting ([`Recorder::is_armed`]) are different states with different
    /// messages; both mean there is no footage to clip.
    fn command_sender(&self) -> Result<Sender<Command>> {
        let guard = self
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_ref() {
            Some(sender) => Ok(sender.clone()),
            None if self.is_armed() => Err(anyhow::anyhow!(
                "no recording is running: this recorder is watching for a game to start \
                 ([games] auto_record = true), so there is nothing to clip yet"
            )),
            None => Err(anyhow::anyhow!("the recorder is not running")),
        }
    }

    /// Stop recording: flush the encoder, close the sources, join the loop, and in
    /// [full-session mode](RecordingMode::FullSession) concatenate the session and close its row first.
    ///
    /// Idempotent, and safe from a shared `Arc<Recorder>`: a concurrent stop waits for the shutdown
    /// rather than returning early. In games mode it also stops the watcher. Returns the failure the
    /// loop stopped for, including a session that could not be concatenated (its segments are kept and
    /// its row left open for the next start; see [`session::recover_sessions`]).
    pub fn stop(&self) -> Result<()> {
        // Supervisor first: it stops a game-driven recording and joins that thread. Take the handle
        // under the lock and join outside it so two concurrent stops wait on the same join.
        let supervisor = self
            .supervisor
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(supervisor) = supervisor {
            supervisor.shutdown();
        }

        // Then a recording this `Recorder` started itself. The guard is held across the join so a
        // concurrent stop cannot see the handle and return before the thread is gone.
        let mut guard = self
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(thread) = guard.take() {
            // A disconnected loop has already stopped; `send` failing is not an error.
            let command = self
                .commands
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
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

    /// Whether the loop thread is still running; `false` for an armed recorder waiting for a game
    /// (see [`Recorder::is_armed`]).
    pub fn is_running(&self) -> bool {
        self.status.running.load(Ordering::SeqCst)
    }

    /// Whether a game watcher is running and a recording will begin when a watched game does
    /// (`[games] auto_record` was on at start).
    pub fn is_armed(&self) -> bool {
        self.status.watching.load(Ordering::SeqCst)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Never leave a capture session and an ffmpeg child behind a dropped handle; a drop cannot
        // report an error, and the status already holds it.
        if let Err(err) = self.stop() {
            tracing::warn!("the recorder did not shut down cleanly: {err:#}");
        }
    }
}

/// How long the supervisor waits for a presence change before checking its stop flag; bounds how long a [`Recorder::stop`] can wait.
const SUPERVISOR_TICK: Duration = Duration::from_millis(200);

/// One microphone block, in ms; matches `MICROPHONE_FORMAT` and the encoder's `MicAudioSpec`.
const MIC_BLOCK_MS: u64 = 10;

/// Everything a start resolved before a recording begins, reusable by a second recording (a game
/// watcher records one session per game, repeating only the handshake that does not touch the display).
struct Prepared {
    cfg: RecorderConfig,
    opts: RecorderOptions,
    codec: VideoCodec,
    vendor: Option<Vendor>,
    /// Taken by a recording when it begins and given back when it ends: one SQLite connection
    /// (`Send` but not `Sync`), moved between the engine thread and the games-mode supervisor.
    ///
    /// Given back on **every** failure too, not only when a recording ends. The store is the
    /// recorder's only connection to the index, and a start that failed after taking it would
    /// leave the recorder with none: the next game would fail at once with "this recorder's
    /// store is already in a recording", and auto-record would be dead for the life of the
    /// process after a single transient failure (a probe timeout, a locked database, a capture
    /// session another application holds).
    store: Option<Store>,
    /// The shared scratch directory (ring mode's segments, and where the clip path looks).
    scratch_dir: PathBuf,
    /// The clips directory both modes write clips into.
    clips_dir: PathBuf,
    /// The sessions area: session mode's segment directories and concatenated files.
    sessions_dir: PathBuf,
    /// A test hook: the starts that must fail (see [`FailStarts`]).
    #[cfg(test)]
    fail_starts: FailStarts,
}

/// A recording that is running: where its commands go, and the thread that runs it.
struct Active {
    commands: Sender<Command>,
    /// The engine thread, which hands the store back when it ends.
    thread: JoinHandle<Option<Store>>,
}

/// The recorder's index connection, on its way into a recording.
///
/// A recording takes the store out of [`Prepared`] and gives it back when it ends. This is the
/// way back for the *other* ending: a start that failed after taking it. The guard holds the
/// store until the recording takes it, and puts it back when the start fails — so the next
/// recording finds a store instead of a recorder that can never record again.
struct StoreGuard<'a> {
    /// The store, until the recording takes it.
    store: Option<Store>,
    /// Where it goes back to.
    slot: &'a mut Option<Store>,
}

impl StoreGuard<'_> {
    /// The store, for the steps that need it before the recording owns it.
    fn store(&self) -> Result<&Store> {
        self.store
            .as_ref()
            .context("this recording already has the store")
    }

    /// The store, for the recording that takes it.
    fn take(&mut self) -> Result<Store> {
        self.store
            .take()
            .context("this recording already has the store")
    }
}

impl Drop for StoreGuard<'_> {
    fn drop(&mut self) {
        // Only when the recording did not take it: whoever is left holding it keeps it.
        if let Some(store) = self.store.take() {
            *self.slot = Some(store);
        }
    }
}

impl Prepared {
    /// The startup handshake for one recording: steps 4 to 7 of [`Recorder::start_with_options`].
    ///
    /// `game` is the watched game that triggered it (written to the session row); `measure` is the
    /// throughput probe (the caller's, or the shipping one when a watcher starts it later).
    fn begin(
        &mut self,
        game: Option<String>,
        measure: &dyn Fn(&EncodeConfig) -> Result<ThroughputMeasurement>,
        status: &Arc<SharedStatus>,
    ) -> Result<Active> {
        let store = self
            .store
            .take()
            .context("this recorder's store is already in a recording")?;
        // The store is out of `self` from here on, so every failure below has to give it back.
        // The guard does that: it hands the store to `returned` when the recording that took it
        // failed, and this function puts it back where the next recording finds it.
        let mut returned: Option<Store> = None;
        let result = {
            let mut guard = StoreGuard {
                store: Some(store),
                slot: &mut returned,
            };
            self.begin_recording(&mut guard, game, measure, status)
        };
        if let Some(store) = returned {
            self.store = Some(store);
        }
        result
    }

    /// The handshake itself, with the store in hand.
    ///
    /// Everything that can fail happens here, and the store moves into the engine only at the
    /// very end — so a failure at any point gives it back (see [`StoreGuard`]).
    fn begin_recording(
        &mut self,
        guard: &mut StoreGuard<'_>,
        game: Option<String>,
        measure: &dyn Fn(&EncodeConfig) -> Result<ThroughputMeasurement>,
        status: &Arc<SharedStatus>,
    ) -> Result<Active> {
        // The test hook fires here, with the store already taken: that is the point of it (see
        // `FailStarts`).
        #[cfg(test)]
        if let Some(why) = self.fail_starts.fail() {
            bail!("{why}");
        }
        // Cloned once per recording to keep the borrow checker out of the rest of this function.
        let cfg = self.cfg.clone();
        let mode = self.opts.mode;
        let buffer_cfg = BufferConfig {
            pre_ms: cfg.buffer.pre_seconds.saturating_mul(1000),
            post_ms: cfg.buffer.post_seconds.saturating_mul(1000),
            segment_ms: cfg.buffer.segment_time.saturating_mul(1000),
            clips_dir: self.clips_dir.clone(),
        };

        // Ring mode uses the shared scratch directory. Session mode uses its own directory under the
        // sessions area, made with `create_dir` (fails rather than overwrites) so two processes starting
        // in the same second cannot mix footage. `now_ms()` is read once and decides the row's
        // `started_at`, the directory name and the final file's name.
        let session_started_ms = now_ms();
        // A replay buffer that also saves the session keeps a directory of its own for the session file.
        let save_session = self.opts.save_session && !mode.is_full_session();
        let own_dir = mode.is_full_session() || save_session;
        let segment_dir = if own_dir {
            session::create_session_dir(&self.sessions_dir, session_started_ms)?
        } else {
            self.scratch_dir.clone()
        };
        // A new recording's failure state is its own: the last game's error must not make this one look failed.
        *status
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;

        // Create the microphone before the capture backend: creating the video backend opens a WGC
        // session on the user's monitor, and a recording that cannot start its microphone must not touch
        // the display first. This is the fail-loud half of `[mic] enabled`: a mic input nothing feeds
        // would record a silent voice track.
        let mut mic = if self.opts.mic.enabled {
            Some(
                build_microphone(cfg.sources, &self.opts.mic.device).with_context(|| {
                    "mic.enabled = true, but the microphone backend could not be created; \
                 disable the microphone or fix the capture device"
                })?,
            )
        } else {
            None
        };
        let microphone = mic.is_some();

        // Only now create the capture backend: the rawvideo pipe is declared from its frame geometry
        // (`native_size`), so the size-dependent encoder config waits for it.
        // Game-audio presence is asked of the configuration once (`game_audio`), here and in the encoder config.
        let game_audio = cfg.game_audio();
        // Tracks are named by position and a one-track container's track is `Game Audio`
        // (`replayed_media::edit::audio_titles`), so a microphone without game audio would be
        // mislabelled. Refused here before anything opens; the encoder refuses it too as a backstop.
        if !game_audio && self.opts.mic.enabled {
            bail!(
                "[audio] enabled = false with [mic] enabled = true cannot be recorded: this build \
                 names a container's first audio track `Game Audio`, so the microphone would be \
                 carried under the name of a track it does not have. Turn the microphone off for a \
                 video-only recording, or leave `[audio] enabled = true`."
            );
        }
        let (mut capture, mut audio) = build_sources(
            cfg.sources,
            &cfg.capture,
            &self.opts.games.watch,
            cfg.encode.fps,
            game_audio,
        )?;
        let native = capture.native_size();

        let mut encode_cfg =
            build_encode_config(&cfg, &segment_dir, self.codec, self.vendor, native)?;
        if microphone {
            // Enable the encoder's second audio input; the default spec is the same 48kHz stereo s16le as
            // `MICROPHONE_FORMAT` and the game-audio input.
            encode_cfg.mic_audio = Some(MicAudioSpec::default());
        }

        // The rate decision. The probe measures the configuration that declares the configured rate,
        // and the rate it reports is what the whole pipeline then uses.
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

        // The two modes build ledger and encoder in opposite, forced orders:
        // * a session's ring indexes files the encoder is about to write, and the encoder's start
        //   segment number is a ffmpeg argument, so the ring comes first;
        // * a replay buffer's ring is fed by the encoder's stdout, so the encoder comes first.
        // Adoption and numbering live in the session arm: they are meaningless for an in-memory ring.
        let mut ledger;
        let encoder;
        let encoder_name;
        match mode {
            RecordingMode::ReplayBuffer => {
                // Fragmented MP4 on stdout: no file is written while buffering. `scratch_dir` is left
                // unconsumed (it is session mode's segment directory).
                encode_cfg.output = replayed_encoder::EncodeOutput::FragmentedStream;
                let (mut enc, name) = spawn_encoder(&cfg.bin, &encode_cfg)?;
                let stream = enc.take_output_stream().context(
                    "a fragmented-stream encoder must expose its output pipe, or the ring \
                     cannot be fed — the encoder and the ring have to agree about the output",
                )?;
                ledger = Ledger::Memory(MemoryRing::start(
                    stream,
                    memory_ring::RingSetup {
                        bin: cfg.bin.clone(),
                        clips_dir: buffer_cfg.clips_dir.clone(),
                        // The RAM safety net that keeps the OS from killing this process.
                        ram_cap_bytes: cfg.buffer.ram_cap_bytes,
                        // The duration cap is what a trigger can ask for: the window plus one segment, so an unlucky
                        // fragment boundary cannot truncate a clip. The buffer's length is the user's `pre_seconds`.
                        cap_ms: cfg
                            .buffer
                            .max_pre_ms()
                            .saturating_add(cfg.buffer.max_post_ms())
                            .saturating_add(buffer_cfg.segment_ms),
                        pre_ms: buffer_cfg.pre_ms,
                        post_ms: buffer_cfg.post_ms,
                        encoder: encode_cfg.encoder_name().to_string(),
                        session_part: save_session
                            .then(|| segment_dir.join(memory_ring::SESSION_PART)),
                    },
                )?);
                encoder = enc;
                encoder_name = name;
            }
            RecordingMode::FullSession => {
                ledger = Ledger::Session(session::SessionRing::open(
                    &cfg.bin,
                    &segment_dir,
                    &buffer_cfg,
                    microphone,
                    encode_cfg.encoder_name().to_string(),
                )?);
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
                let (enc, name) = spawn_encoder(&cfg.bin, &encode_cfg)?;
                encoder = enc;
                encoder_name = name;
            }
        }
        tracing::info!("encoding with {encoder_name}");
        // Which clock this recording's media timeline runs on, and how far it is from the wall
        // clock. The two are not the same clock, and the difference is not zero: the RAM ring
        // measures its fragments, a session's ledger is segment-aligned, and a session's span
        // also lags the wall clock by the segment ffmpeg is still appending to.
        tracing::info!(
            "media clock: {kind}; the ledger stands at {span}ms of media, {elapsed}ms of wall \
             clock have passed since the recording began (one segment is {segment}ms)",
            kind = match mode {
                RecordingMode::ReplayBuffer => "measured from the ring's own fragments",
                RecordingMode::FullSession => "nominal, aligned to the segment length",
            },
            span = ledger.span_ms().unwrap_or(0),
            elapsed = now_ms().saturating_sub(session_started_ms),
            segment = buffer_cfg.segment_ms,
        );
        *status.encoder.lock().unwrap_or_else(|p| p.into_inner()) = Some(encoder_name.clone());
        // Read the pacer's rate back out of the spawned encoder rather than `encode_cfg.fps` a second
        // time, so the pacer cannot pace to a number the child was not told. Read before the encoder
        // moves into the engine.
        let encoder_fps = encoder.input_fps();
        // Second mic check: `mic_audio` set and a port existing are two halves of one fact; a mismatch
        // would claim a track nothing feeds.
        let mic_port = encoder.mic_port();
        if microphone && mic_port.is_none() {
            bail!(
                "the encoder was spawned without a microphone input, so this recording would \
                 carry a voice track nothing feeds (the encode configuration asked for one)"
            );
        }
        // Capture-geometry summary; the rate is the one the pipeline runs at (the adaptation line says why if lower).
        tracing::info!(
            "capture geometry {}x{} at {}fps (frame counter starts at 0){}",
            native.0,
            native.1,
            encoder_fps,
            match (game_audio, microphone) {
                (true, true) => format!(", plus a microphone track on port {mic_port:?}"),
                (true, false) => String::new(),
                // Refused above: see the `[mic]`/`[audio]` check.
                (false, true) => unreachable!("a microphone without the game audio is refused"),
                (false, false) => ", video only: no audio track".to_string(),
            }
        );
        // Write the session row before capturing anything, so a crash mid-recording leaves a row for the
        // next start to find; retention manages the segment directory through it.
        // `started_at` is the wall clock (ages the session, named the directory); `media_epoch_ms` is where
        // the ledger stands now, the origin of an event's `at`. Using the wall clock as the epoch put
        // events of a session that adopted older segments in the wrong place on the scrubber.
        let media_epoch_ms = (ledger.origin_ms().saturating_add(ledger.span_ms()?)) as i64;
        let session = match guard.store()?.start_session(
            game.as_deref(),
            if save_session {
                RecordingMode::FullSession.store_mode()
            } else {
                mode.store_mode()
            },
            session_started_ms,
            &segment_dir.display().to_string(),
            media_epoch_ms,
        ) {
            Ok(id) => id,
            Err(err) => {
                // Nothing was captured; remove the directory created for it.
                if own_dir {
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
            if own_dir {
                " (the RAM cap does not apply)"
            } else {
                ""
            }
        );

        // Start the sources: microphone first (its failure is invisible in the picture), then the
        // display, then game audio if present. On failure everything started is stopped and the session row closed.
        let started = (|| -> Result<()> {
            if let Some(mic) = mic.as_mut() {
                mic.start().context("starting the microphone")?;
            }
            capture.start().context("starting screen capture")?;
            if let Some(audio) = audio.as_mut() {
                audio.start().context("starting audio capture")?;
            }
            Ok(())
        })();
        if let Err(err) = started {
            if let Some(mic) = mic.as_mut() {
                let _ = mic.stop();
            }
            let _ = capture.stop();
            if let Some(audio) = audio.as_mut() {
                let _ = audio.stop();
            }
            if let Ok(store) = guard.store() {
                let _ = store.end_session(session, now_ms(), None, 0, 0);
            }
            if own_dir {
                session::remove_session_dir(&segment_dir);
            }
            return Err(err);
        }

        let (commands, inbox) = mpsc::channel();
        // The store moves into the engine here, and nowhere earlier: every failure above it
        // still finds the store in the guard, and gives it back (see `StoreGuard`).
        let engine = Engine {
            pre_ms: buffer_cfg.pre_ms,
            post_ms: buffer_cfg.post_ms,
            max_pre_ms: cfg.buffer.max_pre_ms(),
            max_post_ms: cfg.buffer.max_post_ms(),
            // The length of one segment, in ms. The post-roll budget is sized from it: a
            // ledger trusts a segment only once a later one exists, so its span lags the wall
            // clock by up to a whole segment (see `POST_ROLL_MARGIN`).
            segment_ms: buffer_cfg.segment_ms,
            // Only the RAM budget is carried: a replay buffer has no scratch directory to cap.
            ram_cap_bytes: cfg.buffer.ram_cap_bytes,
            storage: cfg.storage.clone(),
            cleanup: CleanupReport::default(),
            // Paced to the rate read back from the encoder itself (see `Recorder::start_with_measure`).
            pacer: FramePacer::new(encoder_fps),
            capture,
            audio,
            mic,
            microphone,
            encoder,
            ledger,
            mode,
            session_dir: save_session.then(|| segment_dir.clone()),
            session,
            session_started_ms,
            sessions_dir: self.sessions_dir.clone(),
            size_warning_reported: false,
            encoder_name,
            store: guard.take()?,
            clock: CaptureClock::new(),
            status: Arc::clone(status),
            rate: RateMeter::new(RATE_WINDOW),
            configured_fps: decision.configured(),
            effective_fps: encoder_fps,
            frames: 0,
            skipped: 0,
            capture_time: Duration::ZERO,
            submit_time: Duration::ZERO,
            pump_started: Instant::now(),
            achieved: 0.0,
            last_counted_dropped: 0,
            last_warned_dropped: 0,
            last_drop_warning: None,
            // The clips still being spliced, off the pump thread.
            pending_clips: Vec::new(),
        };
        status.publish_recording(decision.configured(), encoder_fps, mic_port);
        status.set_game(game);
        status.running.store(true, Ordering::SeqCst);

        // The engine is handed to its thread through a slot rather than moved into the closure:
        // a spawn that fails then hands the engine back, and with it the store. Losing the
        // store would leave the recorder with no way to index anything, for the rest of the
        // process (see `StoreGuard`).
        let slot = Arc::new(Mutex::new(Some(engine)));
        let taken = Arc::clone(&slot);
        let thread = match std::thread::Builder::new()
            .name("replayed-recorder".to_string())
            .spawn(move || {
                // The engine is in the slot before the thread starts, so the `None` case cannot
                // happen: it would mean a thread with nothing to run.
                taken
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                    .map(|engine| engine.run(inbox))
            }) {
            Ok(thread) => thread,
            Err(err) => {
                // The engine is still in the slot: take the store back out of it.
                if let Some(engine) = slot.lock().unwrap_or_else(|p| p.into_inner()).take() {
                    let Engine { store, .. } = engine;
                    self.store = Some(store);
                }
                status.running.store(false, Ordering::SeqCst);
                return Err(err).context("spawning the recording thread");
            }
        };

        Ok(Active { commands, thread })
    }
}

/// The game watcher's driver: one recording per game session, in the configured mode.
///
/// Owns the presence changes, the watcher's handle (dropping it stops the watcher), the prepared
/// start state and the running recording. Detection belongs to `replayed-events`; what a
/// detection means for recording is here.
struct Supervisor {
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Supervisor {
    /// Ask the loop to stop and wait for it, including the recording it stops on the way out.
    /// Takes `&self` so [`Recorder::stop`] can call it through the mutex.
    fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
            // A poll in flight finishes (bounded by its tick); the join makes "stopped" true.
            let _ = thread.join();
        }
    }
}

/// Start the supervisor thread (see [`Supervisor`]). The channel and watcher come from the caller
/// (production: `GamesSection::start`; tests: their own), so start/stop is testable without a process list.
fn spawn_supervisor(
    prepared: Prepared,
    presence: Receiver<PresenceChange>,
    watch: Option<WatchHandle>,
    commands: Arc<Mutex<Option<Sender<Command>>>>,
    status: Arc<SharedStatus>,
) -> Result<Supervisor> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    // Armed as this returns, so "is this recorder watching?" right after `start` is not a race.
    status.watching.store(true, Ordering::SeqCst);
    let thread = std::thread::Builder::new()
        .name("replayed-games".to_string())
        .spawn(move || {
            // Kept alive for the whole loop: dropping the handle stops the watcher.
            let _watch = watch;
            let mut prepared = prepared;
            tracing::info!(
                "watching for a game to start: recording in {} mode when one does, and \
                 nothing before that (games.auto_record = true)",
                prepared.opts.mode
            );

            let mut running: Option<(String, JoinHandle<Option<Store>>)> = None;
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
                        // The shipping measurement: this recording begins on this thread, not the caller's.
                        let bin = prepared.cfg.bin.clone();
                        let measure = move |encode_cfg: &EncodeConfig| {
                            replayed_encoder::throughput::measure_sustainable_fps(
                                &bin,
                                encode_cfg,
                                replayed_encoder::throughput::PROBE_BUDGET,
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
                                // Reported; the watcher keeps watching (the next game may record, and startup already vetted the encoder).
                                tracing::error!(
                                    "{game} started, but the recording could not start: {err:#}"
                                );
                                *status.error.lock().unwrap_or_else(|p| p.into_inner()) =
                                    Some(format!("{err:#}"));
                            }
                        }
                    }
                    Ok(PresenceChange::Stopped(game)) => {
                        let Some((name, thread)) = running.take() else {
                            continue;
                        };
                        if name != game.name {
                            // A game other than the recorded one stopped; keep recording.
                            running = Some((name, thread));
                            continue;
                        }
                        tracing::info!("{game} stopped: closing the recording");
                        // Clear before the shutdown below, not after: `matched_game` must be `None` while nothing is
                        // recorded, and the engine clears `running` as its shutdown returns, so clearing after the join
                        // leaves a window (widened by the retention pass) where status names a game not being recorded.
                        status.set_game(None);
                        // Clearing the command channel drops the engine's last sender, ending its loop; the join waits for the shutdown work.
                        *commands.lock().unwrap_or_else(|p| p.into_inner()) = None;
                        match thread.join() {
                            Ok(Some(store)) => {
                                prepared.store = Some(store);
                                // The retention pass between game sessions manages the session that just ended.
                                let mut report = CleanupReport::default();
                                if let Some(store) = &prepared.store {
                                    cleanup_pass(
                                        store,
                                        &prepared.cfg.storage,
                                        &prepared.clips_dir,
                                        &mut report,
                                    );
                                }
                            }
                            Ok(None) => tracing::error!(
                                "the recording thread for {game} ended without handing the index \
                                 back; the next game cannot be recorded"
                            ),
                            Err(_) => tracing::error!(
                                "the recording thread for {game} panicked; its session row is \
                                 left for the next start to recover"
                            ),
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    // Nothing is watching any more: stop rather than capture for nobody.
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }

            // Leaving (application shutting down): stop a recording still running.
            if let Some((name, thread)) = running.take() {
                tracing::info!("stopping the recording of {name}: the recorder is shutting down");
                *commands.lock().unwrap_or_else(|p| p.into_inner()) = None;
                if let Ok(Some(store)) = thread.join() {
                    prepared.store = Some(store);
                }
            }
            status.watching.store(false, Ordering::SeqCst);
            status.set_game(None);
            status.running.store(false, Ordering::SeqCst);
        })
        .context("spawning the game supervisor thread")?;
    Ok(Supervisor {
        stop,
        thread: Mutex::new(Some(thread)),
    })
}

/// The microphone backend for the configured sources.
///
/// [`Sources::Platform`] gives real WASAPI on Windows and an error elsewhere (a synthetic stand-in
/// would claim to capture a voice track while recording nothing); [`Sources::Stub`] gives
/// [`StubMicrophone`] for tests, CI and `--dev-stub-sources`.
fn build_microphone(sources: Sources, device: &str) -> Result<Box<dyn AudioBackend>> {
    match sources {
        Sources::Stub(_) => Ok(Box::new(StubMicrophone::new(
            MICROPHONE_FORMAT,
            MIC_BLOCK_MS,
        ))),
        Sources::Platform => replayed_capture::wasapi_mic::microphone_backend(device),
    }
}

/// Log what the crash-recovery pass found, so a start is never silent about a previous run's footage.
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
    /// The ceilings a per-clip [`ClipWindow`] is clamped to (`[buffer] max_*_seconds`).
    max_pre_ms: u64,
    max_post_ms: u64,
    /// The length of one segment, in ms.
    ///
    /// The post-roll budget is sized from it. A ledger trusts a segment only once a strictly
    /// later one exists (ffmpeg appends to the newest segment), so its span lags the wall
    /// clock by up to a whole segment: the post-roll cannot arrive before that, and a budget
    /// that ignored it would time out on every clip of a session with a long segment.
    segment_ms: u64,
    /// The in-memory ring's byte budget, carried so the ring is checked against the configured number.
    ram_cap_bytes: u64,
    storage: StorageSection,
    cleanup: CleanupReport,
    pacer: FramePacer,
    capture: Box<dyn CaptureBackend>,
    /// The game-audio backend; `None` for a video-only recording (`[audio] enabled = false`). Taken
    /// (stopped) during shutdown, hence [`Engine::game_audio`] records the fact separately.
    audio: Option<Box<dyn AudioBackend>>,
    /// The microphone backend, drained by the same pump iteration into the encoder's own second
    /// input queue. Taken (stopped) during shutdown, hence [`Engine::microphone`] records the fact.
    mic: Option<Box<dyn AudioBackend>>,
    /// Whether this recording has a microphone track. Read by the shutdown path, after [`Engine::mic`]
    /// is taken, to choose the concatenation (a second audio track needs `-map 0`).
    microphone: bool,
    encoder: Box<dyn Encoder>,
    /// The segments and how the trigger finds them: the replay ring (evicts to the cap) or the
    /// session's store (never evicts).
    ledger: Ledger,
    /// The mode this recording runs in ([`RecordingMode`]).
    mode: RecordingMode,
    /// The directory holding the session file a replay buffer is also saving; `None` for a buffer alone
    /// and for full-session mode (whose directory is the ledger's).
    session_dir: Option<PathBuf>,
    /// The `sessions` row this recording opened; footage without a row could not be managed.
    session: i64,
    /// Session start on the wall clock (`sessions.started_at`), an i64 ms epoch.
    session_started_ms: i64,
    /// The sessions area, where a session's file is written at stop.
    sessions_dir: PathBuf,
    /// Whether a failed `set_session_size` was already reported: the tick runs every
    /// [`STATUS_INTERVAL`] and a database that cannot take the update deserves one line, not five a second.
    size_warning_reported: bool,
    /// The encoder that actually ran (`h264_nvenc`, `libx264`, ...), recorded on clips and the session file.
    encoder_name: String,
    store: Store,
    clock: CaptureClock,
    status: Arc<SharedStatus>,
    rate: RateMeter,
    /// Video frames submitted to the encoder, including those the post-roll wait submitted (`frames=`).
    frames: u64,
    /// Frames the backend offered that the pacer threw away without the readback
    /// ([`CaptureBackend::discard_pending`]). With `frames=` it accounts for everything the source produced.
    skipped: u64,
    /// What `encode.fps` asked for. Reported, and used by the drop warning to explain a lower rate in
    /// use; never used to pace or configure the encoder ([`Engine::effective_fps`] does that).
    configured_fps: u32,
    /// The rate the pipeline actually runs at (the pacer interval and the encoder's `-framerate`).
    /// The status denominator, and what the drop warning compares the achieved rate against.
    effective_fps: u32,
    /// The last rate a window closed on, published on every pump and logged on every tick.
    achieved: f64,
    /// The clips whose splice is running on its own thread (see [`PendingClip`]).
    pending_clips: Vec<PendingClip>,
    /// Time spent inside the capture backend, accumulated over the run
    /// ([`replayed_recorder::pump::PumpCounts::capture`]): the GPU readback cost, reported against wall clock.
    capture_time: Duration,
    /// Time spent inside the encoder's submit, i.e. waiting for its bounded queue to drain.
    submit_time: Duration,
    /// When the pump's counters started, so the two above can be reported as a share of wall clock.
    pump_started: Instant,
    /// The dropped count as of the last look by the rate meter ([`Engine::last_counted_dropped`])
    /// and by the warning ([`Engine::last_warned_dropped`]); each subtracts a cumulative drop exactly once.
    last_counted_dropped: u64,
    /// The dropped count as of the last warning, to tell "still dropping" from "dropped once, an hour ago".
    last_warned_dropped: u64,
    last_drop_warning: Option<Instant>,
}

/// A clip whose splice is running on its own thread.
///
/// The splice is ffmpeg's, and ffmpeg takes as long as the clip is long. Run on the pump thread
/// it would stop the pump: the capture backend is not drained, the encoder's queue fills and
/// drops the frames it cannot take, and because the media clock is the wall clock the frames
/// that follow the splice carry a timestamp a whole splice later — a frozen picture in the
/// recording, at every clip, for as long as the splice took.
///
/// So the splice runs on a thread of its own and the pump keeps running under it. The caller's
/// contract is unchanged: the answer comes when the file exists.
struct PendingClip {
    /// The caller waiting for the answer.
    reply: Sender<Result<RecordedClip>>,
    /// The splice and the chapter write, on their own thread.
    worker: JoinHandle<Result<ClipMetadata>>,
    /// The trigger's position on the media timeline: when the event happened, not when the
    /// request was handled.
    at_ms: u64,
    /// The clip's first frame on the media timeline, from the resolved window.
    started_at_ms: u64,
    /// Why the clip was taken, for the `events` row.
    reason: ClipReason,
}

/// Clears the running flag when the engine thread ends, however it ends.
///
/// The flag is set before the thread is spawned, so a panic in the loop would otherwise leave
/// the recorder claiming to run for ever: a front-end that polls `status()` would show a
/// recording that no longer exists, and would wait for a stop that never comes.
struct RunningGuard(Arc<SharedStatus>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::SeqCst);
    }
}

/// Splice a clip and write its chapters, on a thread of its own.
///
/// The chapters are written here rather than on the pump thread because they are a second
/// ffmpeg pass over the whole file: leaving them on the pump thread would keep the hole the
/// splice used to punch.
///
/// The events are the ones the pump thread read before the splice started, so a marker
/// recorded while the splice ran is not a chapter of this clip.
fn splice_clip(
    bin: FfmpegBinaries,
    source: ClipSource,
    out: PathBuf,
    encoder: String,
    events: Vec<Event>,
    trigger: Option<(String, u64)>,
    started_at_ms: u64,
) -> Result<ClipMetadata> {
    let mut clip = source.splice(&bin, &out, &encoder)?;
    // The chapters, at the offsets the clip really starts on: the resolved window's start, and
    // the clip's own length rather than the window's.
    let trigger = trigger.as_ref().map(|(kind, at)| (kind.as_str(), *at));
    let chapters = clip_chapters(&events, trigger, started_at_ms, clip.duration_ms);
    if chapters.is_empty() {
        return Ok(clip);
    }
    match edit::add_chapters(&bin, &out, &chapters, clip.duration_ms) {
        Ok(()) => {
            if let Ok(meta) = std::fs::metadata(&out) {
                clip.size_bytes = meta.len();
            }
            tracing::info!("marked {} chapters in {}", chapters.len(), out.display());
        }
        Err(err) => tracing::warn!(
            "could not add chapters to {} ({err:#}); the clip is kept without them",
            out.display()
        ),
    }
    Ok(clip)
}

/// How long a clip request may take: the post-roll wait plus the splice.
///
/// Both are bounded, and the bound is what lets the caller give up instead of waiting for
/// ever on a request whose tail (the index writes) is not bounded at all.
fn clip_budget(cfg: &RecorderConfig) -> Duration {
    let segment_ms = cfg.buffer.segment_time.saturating_mul(1000);
    // The post-roll wait, as the engine sizes it: the longest post-roll the configuration
    // allows, plus the segment lag the ledger carries, plus the margin.
    let post_roll = Duration::from_millis(
        cfg.buffer
            .max_post_ms()
            .saturating_add(segment_ms.saturating_mul(2))
            .saturating_add(POST_ROLL_MARGIN.as_millis() as u64),
    );
    // The splice: a stream copy of the biggest window the configuration can ask for, so the
    // budget is derived from the bytes it moves (the media crate's own rule). The estimate is
    // doubled: the encoder's rate control overshoots its target (measured at ~1.9x), and a
    // budget that expires while the copy is still running would report a failure for a clip
    // that was about to exist.
    let window_ms = cfg
        .buffer
        .max_pre_ms()
        .saturating_add(cfg.buffer.max_post_ms());
    let window_bytes = window_ms
        .saturating_mul(u64::from(cfg.encode.bitrate_kbps))
        .saturating_mul(2)
        .saturating_div(8)
        .saturating_div(1000);
    post_roll + edit::copy_budget(window_bytes)
}

/// The segments a recording's trigger and shutdown work against.
///
/// Two shapes, one timeline: the in-memory ring is a bounded window that drops its oldest footage
/// when full; the full session's store never evicts (see [`crate::session`]).
enum Ledger {
    /// The rolling replay buffer, held in RAM so an idle buffer writes nothing to the SSD. A crash
    /// loses it; session mode ([`Ledger::Session`]) keeps files because a whole recording is worth surviving one.
    Memory(MemoryRing),
    /// A full session's segment directory.
    Session(crate::session::SessionRing),
}

impl Ledger {
    /// Find newly written segments; a no-op for the memory ring, which is fed by its own reader thread.
    fn scan(&mut self) -> Result<()> {
        match self {
            Ledger::Memory(_) => Ok(()),
            Ledger::Session(session) => session.scan(),
        }
    }

    /// Index every segment, the newest included. Called at shutdown after the encoder has exited, so
    /// the last file is complete (the live scan does not trust the newest file while ffmpeg appends
    /// to it). Meaningless for the memory ring, whose reader already ingested everything.
    fn scan_all(&mut self) -> Result<()> {
        match self {
            Ledger::Memory(_) => Ok(()),
            Ledger::Session(session) => session.scan_all(),
        }
    }

    /// What this ring holds, in the shape the status line uses.
    ///
    /// `bytes_on_disk` is left 0 for the memory ring (RAM is reported by [`Ledger::buffered_bytes`]),
    /// so the sessions row's `size_bytes`, a disk claim, never becomes a memory claim. Fallible: a ring
    /// that cannot be read must not report zero footage as if it were empty.
    fn stats(&self) -> Result<BufferStats> {
        Ok(match self {
            Ledger::Memory(ring) => BufferStats {
                segments: ring.stats()?.segments,
                bytes_on_disk: 0,
                span_ms: ring.span_ms()?,
            },
            Ledger::Session(session) => BufferStats {
                segments: session.segment_count(),
                bytes_on_disk: session.bytes_on_disk(),
                span_ms: session.span_ms(),
            },
        })
    }

    /// Bytes the ring holds wherever it holds them (disk for a session, RAM for a replay buffer):
    /// the single `bytes=` field means "how much footage is buffered"; the medium follows from the mode.
    fn buffered_bytes(&self) -> Result<u64> {
        match self {
            Ledger::Memory(ring) => ring.bytes(),
            Ledger::Session(session) => Ok(session.bytes_on_disk()),
        }
    }

    /// Run-relative media time the ring holds, in ms.
    fn span_ms(&self) -> Result<u64> {
        Ok(self.stats()?.span_ms)
    }

    /// Fragments the ring has dropped off the front to stay inside its caps.
    ///
    /// Zero for a session: a session's segments are written to disk and the storage policy, not the
    /// ring, decides what is deleted. For the memory ring this is the count of fragments evicted
    /// oldest-first to stay inside `buffer.ram_cap_bytes` or the pre-roll window — the number that
    /// says the cap *bit* rather than merely that it was never reached.
    fn evicted(&self) -> Result<u64> {
        match self {
            Ledger::Memory(ring) => Ok(ring.stats()?.evicted),
            Ledger::Session(_) => Ok(0),
        }
    }

    /// The ledger's own position of this run's zero.
    fn origin_ms(&self) -> u64 {
        match self {
            // A memory ring starts at zero every run; nothing is adopted.
            Ledger::Memory(ring) => ring.origin_ms(),
            Ledger::Session(session) => session.origin_ms(),
        }
    }

    /// The pump's view of this ledger, for the post-roll wait.
    fn as_ring(&mut self) -> &mut dyn MediaRing {
        match self {
            Ledger::Memory(ring) => ring,
            Ledger::Session(session) => session,
        }
    }

    /// Index what is already on disk before the encoder is spawned. A memory ring adopts nothing.
    fn adopt_existing(&mut self) -> Result<usize> {
        match self {
            Ledger::Memory(_) => Ok(0),
            Ledger::Session(session) => session.adopt_existing(),
        }
    }

    /// Reserve the sequence number the encoder must start at; zero for the memory ring (no files).
    fn reserve_number(&mut self) -> Result<u64> {
        match self {
            Ledger::Memory(_) => Ok(0),
            Ledger::Session(session) => session.reserve_number(),
        }
    }

    fn bin(&self) -> &FfmpegBinaries {
        match self {
            Ledger::Memory(ring) => ring.bin(),
            Ledger::Session(session) => session.bin(),
        }
    }

    /// The directory the clips are written into.
    fn clips_dir(&self) -> &Path {
        match self {
            Ledger::Memory(ring) => ring.clips_dir(),
            Ledger::Session(session) => session.clips_dir(),
        }
    }

    /// The footage a clip around `trigger_ms` is cut from, and where that footage starts.
    ///
    /// The window is resolved here, not at the splice: the resolution reads the ledger, and
    /// the ledger belongs to this thread. What comes back owns its data — the ring's bytes are
    /// copied out from under its lock, a session's segments are file names — so the splice can
    /// run on another thread.
    fn clip_source(&self, trigger_ms: u64, pre_ms: u64, post_ms: u64) -> Result<ClipSource> {
        match self {
            Ledger::Memory(ring) => ring.clip_source(trigger_ms, pre_ms, post_ms),
            Ledger::Session(session) => session.clip_source(trigger_ms, pre_ms, post_ms),
        }
    }
}

/// A clip's footage, lifted out of the ledger so the splice can run on another thread.
///
/// The RAM ring's bytes are copied out from under the ring's lock and the session's segments
/// are file names, so neither borrows the ledger: both are theirs to take to a worker.
pub(crate) enum ClipSource {
    /// The ring's fragments, assembled into one fragmented-MP4 stream.
    Stream {
        bytes: Vec<u8>,
        /// How many audio tracks the footage carries, so the copy can name them again.
        audio_tracks: usize,
        /// Where the clip's first frame is, on the media timeline.
        start_ms: u64,
        /// How much media the window covers.
        duration_ms: u64,
    },
    /// The session's whole segments, concatenated losslessly.
    Segments {
        window: replayed_replay::window::SegmentWindow,
        /// Whether the session carries a microphone track: the concatenation must keep it.
        multitrack: bool,
    },
}

impl ClipSource {
    /// Where the clip's first frame is, on the media timeline.
    ///
    /// The *resolved* window's start, not the requested one: the window selects whole segments
    /// and fragments, so the first selected one begins at or before the request. Dating a clip
    /// by the request is off by up to one segment, and the index row and the chapter offsets
    /// both carry that error.
    fn start_ms(&self) -> u64 {
        match self {
            ClipSource::Stream { start_ms, .. } => *start_ms,
            ClipSource::Segments { window, .. } => window.start_ms(),
        }
    }

    /// How much media the window covers.
    fn duration_ms(&self) -> u64 {
        match self {
            ClipSource::Stream { duration_ms, .. } => *duration_ms,
            ClipSource::Segments { window, .. } => window.duration_ms(),
        }
    }

    /// The clip's file, written losslessly.
    ///
    /// The RAM path remuxes the assembled stream; the session path concatenates its whole
    /// segments. Both are `-c copy`, and both may run on a thread of their own.
    fn splice(self, bin: &FfmpegBinaries, out: &Path, encoder: &str) -> Result<ClipMetadata> {
        match self {
            ClipSource::Stream {
                bytes,
                audio_tracks,
                ..
            } => ClipSplicer::splice_stream(bin, bytes, out, encoder, audio_tracks),
            ClipSource::Segments { window, multitrack } => {
                if multitrack {
                    // A clip from a session with a microphone must keep that track, like the
                    // session file (see `session::finalise`).
                    session::finalise(bin, out, &window.segments, encoder, true)
                } else {
                    ClipSplicer::splice(bin, &window, out, encoder)
                }
            }
        }
    }
}

impl Engine {
    /// The pump loop. Returns when told to stop, when the last `Recorder` is dropped, or when the
    /// capture/encode path fails, and hands the store back (a game-driven recorder reuses the connection).
    ///
    /// Everything is closed before it returns: encoder flushed, sources stopped, `sessions` row
    /// ended and, in full-session mode, the session file written.
    fn run(mut self, commands: Receiver<Command>) -> Store {
        // The running flag is cleared when this returns, however it returns: it is set before
        // the thread is spawned, so a panic in the loop would otherwise leave the recorder
        // claiming to run for ever.
        let _running = RunningGuard(Arc::clone(&self.status));
        // Tell capture what the encoder accepts (textures or pixels) before it starts; capture remembers
        // the answer for the session it builds. One question with one answer: a mismatch leaves an
        // encoder with pixels it refuses or a texture it cannot use.
        self.capture
            .deliver_textures(self.encoder.accepts_textures());

        let mut last_scan = Instant::now();
        // Next storage-policy pass, in wall clock from the last one.
        let mut last_cleanup = Instant::now();

        loop {
            match commands.try_recv() {
                Ok(Command::Stop) => break,
                Ok(Command::Clip {
                    reason,
                    window,
                    event_at_ms,
                    reply,
                }) => {
                    // The splice runs on its own thread, so the failure is reported here and
                    // the clip itself is answered when its file exists.
                    if let Err(err) = self.take_clip(reason, window, event_at_ms, reply.clone()) {
                        let _ = reply.send(Err(err));
                    }
                }
                Ok(Command::Note {
                    event,
                    at_ms,
                    reply,
                }) => {
                    let noted = self.note_event(&event, at_ms);
                    let _ = reply.send(noted);
                }
                Err(TryRecvError::Empty) => {}
                // Every `Recorder` was dropped: stop instead of capturing for nobody.
                Err(TryRecvError::Disconnected) => break,
            }

            // A clip whose splice has finished is answered here, so the caller waits for the
            // file and not for the loop to notice it.
            self.advance_clips();

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

        // A clip that was still being spliced when the loop ended is finished now: the file
        // is written, and so is its index row. The recording is over, but the clip the user
        // asked for exists.
        self.finish_pending_clips();

        // Flush and shut down; best effort on the failure path, since the encoder may be what failed.
        if let Err(err) = self.shutdown() {
            self.fail(err);
        }
        self.store
    }

    /// One pump: submit whatever frame and audio are due.
    fn pump(&mut self) -> Result<()> {
        let counts = pump_once_counted_with_mic(
            &mut self.pacer,
            self.capture.as_mut(),
            self.audio.as_deref_mut(),
            self.mic.as_deref_mut(),
            self.encoder.as_mut(),
        )?;
        self.account(counts);
        Ok(())
    }

    /// The ~200ms tick: scan the ledger, publish and log status, keep the session row's size current,
    /// report how much the ring has evicted, warn about a starving encoder, and run the storage
    /// policy on its own interval.
    fn tick(&mut self, last_cleanup: &mut Instant) -> Result<()> {
        self.ledger.scan().context("scanning for new segments")?;

        let stats = self.ledger.stats()?;
        // `stats.bytes_on_disk` is 0 for a RAM replay buffer, so `bytes=` uses `buffered_bytes` (one
        // read, used twice); otherwise the line reports `bytes=0` for a full ring.
        let buffered = self.ledger.buffered_bytes()?;
        self.status.publish_ring(
            stats.segments as u64,
            // Bytes held wherever they live (RAM or disk), not `stats.bytes_on_disk`.
            buffered,
            stats.span_ms,
        );
        // Update the `sessions` row's size on the tick that measured it, so a multi-hour recording is
        // visible to the sessions retention cap before it stops.
        self.report_session_size(stats.bytes_on_disk);
        let dropped = self.encoder.dropped_frames();

        // `dropped=` / `dropped_audio=` are the encoder's counts of discarded video frames / audio blocks
        // (queue full; see `replayed-encoder`); a drop in audio is a sound hole, so they are separate.
        // `frames=` + `skipped=` is the source's own rate; `fps=` is the achieved rate over `RATE_WINDOW`,
        // printed against the pipeline rate; `configured=` differs from it exactly when the startup probe
        // measured less and adaptation is on. `capture=` is a share of wall clock, split into copy vs wait
        // when the backend measures it; `unmeasured` is the honest answer when it does not.
        let split = match self.capture.frame_timings() {
            Some(timings) => {
                let elapsed = self.pump_started.elapsed().as_secs_f64();
                if elapsed <= 0.0 {
                    "capture_split=unmeasured".to_string()
                } else {
                    format!(
                        "capture_wait={:.1}% capture_copy={:.1}%",
                        100.0 * timings.waiting.as_secs_f64() / elapsed,
                        100.0 * timings.copying.as_secs_f64() / elapsed
                    )
                }
            }
            None => "capture_split=unmeasured".to_string(),
        };

        tracing::debug!(
            "frames={} segments={} bytes={} span={}ms evicted={} dropped={} dropped_audio={} \
             dropped_mic={} skipped={} fps={:.1}/{} configured={} capture={:.1}% submit={:.1}% {}",
            self.frames,
            stats.segments,
            // Same buffered-bytes number the status publishes, so line and panel agree.
            buffered,
            stats.span_ms,
            // Fragments the ring dropped off the front to stay inside its RAM cap. Zero in session
            // mode, where the storage policy does the deleting.
            self.ledger.evicted()?,
            dropped,
            self.encoder.dropped_audio_blocks(),
            self.encoder.dropped_mic_audio_blocks(),
            self.skipped,
            self.achieved,
            self.effective_fps,
            self.configured_fps,
            // Shares of wall clock since the first pump: `capture=` is readback and copy, `submit=` is waiting
            // for ffmpeg, and the rest is audio drains, the tick and the loop.
            Self::share(self.capture_time, self.pump_started.elapsed()),
            Self::share(self.submit_time, self.pump_started.elapsed()),
            split,
        );
        // The ring is held to a RAM cap, not scratch: overrunning it means the OS kills the process. The
        // ring evicts on every push, so this only fires if eviction stopped working, which is worth failing
        // loudly over. A full session has no cap, structurally (its segments are the recording).
        if let Ledger::Memory(ring) = &self.ledger {
            let held = ring.bytes()?;
            if held > self.ram_cap_bytes {
                bail!(
                    "the in-memory ring is over its RAM budget: {held} bytes held against {},                      which means its eviction has stopped working",
                    self.ram_cap_bytes
                );
            }
        }

        // A rising `dropped=` means the encoder queue is overflowing although the pacer admits only the
        // pipeline rate: the machine cannot encode that rate now, despite the startup measurement. Make it
        // loud: each dropped frame holds the previous picture. It is not a timeline warning, since the
        // media clock is the frames' arrival timestamps (`-fps_mode passthrough`, see
        // `replayed_encoder::ffmpeg`) and `span=` keeps tracking the wall clock.
        let dropped_since_last = dropped.saturating_sub(self.last_warned_dropped);
        let warn_due = self
            .last_drop_warning
            .is_none_or(|t| t.elapsed() >= DROP_WARN_INTERVAL);
        // `achieved > 0.0` means a window has closed, so there is a rate to quote. Waiting loses nothing:
        // `dropped_since_last` stays non-zero until a warning is emitted, which also prevents one per log line.
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

        // Periodic storage-policy pass on the same tick; `cleanup_pass` is silent when there is nothing to do.
        if last_cleanup.elapsed() >= CLEANUP_INTERVAL {
            cleanup_pass(
                &self.store,
                &self.storage,
                self.ledger.clips_dir(),
                &mut self.cleanup,
            );
            *last_cleanup = Instant::now();
        }
        Ok(())
    }

    /// The trigger path, run on the loop thread because it pumps.
    ///
    /// The post-roll wait and the window resolution happen here; the splice and the chapter
    /// write do not (see [`PendingClip`]). The caller is answered when the file exists, as
    /// before — only the thread the work happens on has changed.
    fn take_clip(
        &mut self,
        reason: ClipReason,
        window: ClipWindow,
        event_at_ms: Option<u64>,
        reply: Sender<Result<RecordedClip>>,
    ) -> Result<()> {
        let (pre_ms, post_ms) =
            window.resolve(self.pre_ms, self.post_ms, self.max_pre_ms, self.max_post_ms);
        // The trigger is "now" on the ledger's media timeline, not the wall clock. The clocks only agree
        // while the pipeline keeps up, and a wall-clock `trigger_ms` makes `need_ms` unreachable (the
        // ledger can never catch a target derived from a faster clock; the post-roll wait times out).
        // `span_ms` is the end of the footage the ring can prove is on disk, and `need_ms` and the splice
        // window `[trigger_ms - pre_ms, trigger_ms + post_ms]` share that clock, so the target is
        // reachable by construction.
        //
        // `pre_seconds` therefore means seconds of recorded footage; clips splice whole segments, so a
        // clip can run up to one segment past the request but never short of it. If the buffer holds less
        // than `pre_ms`, the ring's `clip_source` warns and splices the truncated front.
        let trigger_ms = self.ledger.span_ms()?;
        // Wall clock, telemetry only (logged beside the media value; `drift` should sit still). Mixing
        // the two clocks is what made the post-roll unreachable, so nothing below reads it.
        let wall_ms = self.clock.ms_at(Instant::now());
        // The line describes the trigger instant, identical for a hotkey, button or game event; the reason adds who asked.
        tracing::info!(
            "{}: media={trigger_ms}ms wall={wall_ms}ms (drift {}ms); waiting for post-roll",
            reason.describe(),
            wall_ms as i64 - trigger_ms as i64
        );

        // Wait for the post-roll to be written before splicing. The budget is the post-roll plus
        // the segment lag plus a margin, never a constant: those media seconds stay invisible to
        // the ring until ffmpeg finishes the segment carrying them (see `POST_ROLL_MARGIN`).
        let need_ms = trigger_ms.saturating_add(post_ms);
        let budget = self.post_roll_budget(post_ms);
        // One wait loop for both modes and the microphone (see `pump_until_span_on`).
        let counts = pump_until_span_on(
            &mut self.pacer,
            self.ledger.as_ring(),
            self.capture.as_mut(),
            self.audio.as_deref_mut(),
            self.mic.as_deref_mut(),
            self.encoder.as_mut(),
            need_ms,
            budget,
        )?;
        self.account(counts);

        // The window, resolved: the footage the clip is cut from, and where it starts. The start
        // is the resolved one — the first segment or fragment that overlaps the request — because
        // that is where the clip's first frame really is. The request is only a bound on it.
        let source = self.ledger.clip_source(trigger_ms, pre_ms, post_ms)?;
        let started_at_ms = source.start_ms();
        // The event's own moment, when the caller knew it. A burst of game events is coalesced
        // before the clip is cut, and the coalescing can run for most of the pre-roll: the
        // request instant is then long after the moment the event describes, and a marker
        // stamped with it would point at the wrong second of the recording.
        let at_ms = match event_at_ms {
            Some(at_ms) => at_ms,
            None => self.ledger.origin_ms().saturating_add(trigger_ms),
        };

        // The events in the clip's span, for its chapters. Read before the splice, so the
        // chapters are known when the file is written: a marker recorded while the splice ran
        // is not a chapter of this clip, which is a fraction of a second of a long recording.
        let events = self.clip_events(started_at_ms, source.duration_ms());
        // The event that asked for the clip, as the chapter's kind and position.
        let trigger = reason
            .event()
            .map(|event| (event.kind.as_tag().to_string(), at_ms));

        let stem = format!("clip-{}", unix_millis());
        let out = self.ledger.clips_dir().join(format!("{stem}.mp4"));
        let bin = self.ledger.bin().clone();
        let encoder = self.encoder_name.clone();
        // The splice, on a thread of its own: ffmpeg takes as long as the clip is long, and the
        // pump must keep running while it does.
        let worker = std::thread::Builder::new()
            .name("replayed-splice".to_string())
            .spawn(move || splice_clip(bin, source, out, encoder, events, trigger, started_at_ms))
            .context("spawning the clip's splice thread")?;
        self.pending_clips.push(PendingClip {
            reply,
            worker,
            at_ms,
            started_at_ms,
            reason,
        });
        Ok(())
    }

    /// How long the post-roll wait may take.
    ///
    /// The post-roll itself, plus twice the segment length, plus the margin. The segment term
    /// is the ledger's trust lag: a segment counts only once a strictly later one exists (the
    /// file ledger's "ffmpeg appends to the newest segment"), so the span lags the wall clock
    /// by up to a whole segment and the post-roll cannot arrive before that. A budget without
    /// the term timed out on every clip of a session with a long segment, and the user's clip
    /// was lost.
    fn post_roll_budget(&self, post_ms: u64) -> Duration {
        Duration::from_millis(
            post_ms
                .saturating_add(self.segment_ms.saturating_mul(2))
                .saturating_add(POST_ROLL_MARGIN.as_millis() as u64),
        )
    }

    /// The events in the clip's span, for its chapters.
    ///
    /// Best effort: a failure is logged and the clip is written without chapters, because the
    /// clip is what the user asked for.
    fn clip_events(&self, started_at_ms: u64, duration_ms: u64) -> Vec<Event> {
        let end_ms = started_at_ms.saturating_add(duration_ms);
        match self
            .store
            .events_in_range(self.session, started_at_ms, end_ms)
        {
            Ok(events) => events,
            Err(err) => {
                tracing::warn!("no chapters for the clip: reading its events failed: {err:#}");
                Vec::new()
            }
        }
    }

    /// Answer the clips that are ready.
    ///
    /// The loop calls this on every pass, so a finished splice is noticed within a frame
    /// interval and the caller is answered without waiting for the recording to stop.
    fn advance_clips(&mut self) {
        let Some(index) = self
            .pending_clips
            .iter()
            .position(|pending| pending.worker.is_finished())
        else {
            return;
        };
        let pending = self.pending_clips.swap_remove(index);
        let reply = pending.reply.clone();
        match self.finish_clip(pending) {
            Ok(clip) => {
                let _ = reply.send(Ok(clip));
            }
            Err(err) => {
                let _ = reply.send(Err(err));
            }
        }
    }

    /// The index writes, once the clip's file exists.
    ///
    /// The file is written by the splice thread; the index is written here, because the store
    /// belongs to this thread and is not shareable.
    fn finish_clip(&mut self, pending: PendingClip) -> Result<RecordedClip> {
        let clip = match pending.worker.join() {
            Ok(Ok(clip)) => clip,
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err(anyhow::anyhow!("the clip's splice thread panicked")),
        };
        tracing::info!(
            "wrote {} ({}ms, {} bytes, encoder={})",
            clip.path.display(),
            clip.duration_ms,
            clip.size_bytes,
            clip.encoder
        );

        let id = index_clip(&self.store, &clip, pending.started_at_ms);
        // The clip carries its recording session's game so the library can filter without a join. Best effort.
        if let Some(id) = id {
            if let Ok(Some(game)) = self
                .store
                .get_session(self.session)
                .map(|s| s.and_then(|s| s.game))
            {
                if let Err(err) = self.store.set_clip_game(id, Some(&game)) {
                    tracing::warn!("could not record the game on clip #{id}: {err:#}");
                }
            }
        }

        // An event-triggered clip says why it exists, and so does a manual one (the user's own bookmark).
        // `at` is the trigger instant (`pre_ms` into the clip), so the marker lands on the moment that
        // caused it. A failed insert is reported, not fatal: the clip file is on disk either way.
        let session_id = Some(self.session);
        let recorded = match pending.reason.event() {
            Some(event) => index_event(&self.store, event, pending.at_ms, id, session_id),
            None => index_bookmark(&self.store, pending.at_ms, id, session_id),
        };
        if let Err(err) = recorded {
            tracing::error!(
                "could not record the event that asked for this clip ({err:#}); the clip \
                 itself is kept, and the session timeline is missing one marker"
            );
        }

        self.status.clips.fetch_add(1, Ordering::Relaxed);
        Ok(RecordedClip {
            metadata: clip,
            id,
            started_at_ms: pending.started_at_ms,
        })
    }

    /// Finish the clips that were still being spliced when the recording ended.
    ///
    /// The recording is over, but the clip the user asked for exists: the file is written and
    /// its index row with it.
    fn finish_pending_clips(&mut self) {
        while let Some(pending) = self.pending_clips.pop() {
            let reply = pending.reply.clone();
            let outcome = self.finish_clip(pending);
            let _ = reply.send(outcome);
        }
    }

    /// Record an event that did not ask for a clip (see [`Recorder::note_event`]).
    ///
    /// `at_ms` is the event's own moment when the caller knew it, and "now" otherwise: the
    /// moment the request was handled is right for a marker nobody coalesced and wrong for one
    /// that waited in a burst.
    fn note_event(&mut self, event: &GameEvent, at_ms: Option<u64>) -> Result<i64> {
        let at_ms = match at_ms {
            Some(at_ms) => at_ms,
            None => self
                .ledger
                .origin_ms()
                .saturating_add(self.ledger.span_ms()?),
        };
        index_event(&self.store, event, at_ms, None, Some(self.session))
    }

    /// Keep the `sessions` row's `size_bytes` current.
    ///
    /// Retention counts `sessions.size_bytes`, so without this a multi-hour recording is invisible to
    /// it until it stops. Runs on the tick that already scanned the ledger; a failure is reported
    /// once, since the tick fires every [`STATUS_INTERVAL`].
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

    /// `part` as a percentage of `whole`, for the periodic line's two attributions.
    ///
    /// A share of wall clock (not process CPU time) so it reads against the OS's CPU figure; a
    /// zero-length window (the first tick) reports 0.
    fn share(part: Duration, whole: Duration) -> f64 {
        let whole = whole.as_secs_f64();
        if whole <= 0.0 {
            0.0
        } else {
            100.0 * part.as_secs_f64() / whole
        }
    }

    /// Fold one pump's counts into the counters and publish them.
    fn account(&mut self, counts: PumpCounts) {
        self.frames += counts.submitted;
        self.skipped += counts.skipped;
        // Accumulated where every pump's counts pass: the steady-state loop and the post-roll wait.
        self.capture_time += counts.capture;
        self.submit_time += counts.submit;
        // Where the recording stands on the media timeline, published for the callers that stamp
        // an event with the moment it happened (see `Recorder::media_now_ms`).
        self.publish_media_position();
        // Frames that reached ffmpeg: submitted minus what the bounded queue dropped. This, not `frames=`,
        // is the achieved rate. The count is cumulative, so only new drops come out of the meter.
        let dropped = self.encoder.dropped_frames();
        let reached = counts
            .submitted
            .saturating_sub(dropped.saturating_sub(self.last_counted_dropped));
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
        // The mic's drop counter shares the status surface: a voice track quietly losing blocks must be visible.
        self.status
            .publish_mic(self.encoder.dropped_mic_audio_blocks());
    }

    /// Wall clock minus media time, in ms.
    fn drift_ms(&self) -> i64 {
        // Telemetry only: an unreadable ring contributes no media position, so the result is the wall
        // clock alone, which is visibly wrong rather than quietly plausible (the reader's error is logged once).
        self.clock.ms_at(Instant::now()) as i64 - self.ledger.span_ms().unwrap_or(0) as i64
    }

    /// Publish where the recording stands on the media timeline.
    ///
    /// The ledger's position, not the wall clock's: the footage is measured on the ledger's
    /// clock, so an event stamped with it lands next to the clip that covers it. A ledger that
    /// cannot be read contributes no position, and the published value stays where it was: a
    /// stale position is better than a jump to zero.
    fn publish_media_position(&self) {
        let Ok(span) = self.ledger.span_ms() else {
            return;
        };
        self.status.media_ms.store(
            self.ledger.origin_ms().saturating_add(span),
            Ordering::Relaxed,
        );
    }

    /// Flush the encoder, close the sources, and close the session.
    ///
    /// Every step is attempted even if an earlier one failed, so a failure never leaves the display
    /// captured or ffmpeg writing.
    fn shutdown(&mut self) -> Result<()> {
        // The encoder first: its last segment must be closed before the ledger is scanned, or it is never indexed.
        let flushed = self.encoder.finish().context("flushing the encoder");
        let scanned = if flushed.is_ok() {
            self.ledger
                .scan()
                .context("scanning for new segments at shutdown")
        } else {
            Ok(())
        };
        let ledger = match &mut self.ledger {
            // Nothing on disk to save: the replay buffer lives in RAM. The reader is joined here because the
            // flushed encoder's child has exited, so the stream has ended; an early-stopped reader is reported.
            Ledger::Memory(ring) => ring.join(),
            // A session writes no ledger file: its segments are the directory listing, which makes them recoverable.
            Ledger::Session(_) => Ok(()),
        };
        // Sources close before the session is concatenated: a multi-gigabyte copy must not keep the display under a WGC session.
        let audio = match &mut self.audio {
            Some(audio) => audio.stop().context("stopping audio capture"),
            // A video-only recording has no audio backend to stop.
            None => Ok(()),
        };
        let capture = self.capture.stop().context("stopping screen capture");
        let mic = match self.mic.take() {
            Some(mut mic) => mic.stop().context("stopping the microphone"),
            None => Ok(()),
        };
        let session = self.finish_session();
        // Every step ran, and every failure is reported. `Result::and` keeps only the *first*
        // error, and the one the recovery pass needs is often the last: a session that could
        // not be concatenated was invisible whenever the encoder flush also failed.
        let failures: Vec<String> = [flushed, scanned, ledger, audio, capture, mic, session]
            .into_iter()
            .filter_map(|step| step.err().map(|err| format!("{err:#}")))
            .collect();
        if failures.is_empty() {
            return Ok(());
        }
        Err(anyhow::anyhow!(
            "the recording did not shut down cleanly: {}",
            failures.join("; ")
        ))
    }

    /// Close this recording's `sessions` row, concatenating a full session first.
    ///
    /// * Buffer mode: the row is closed with the bytes the ring holds and no file.
    /// * Full-session mode: the segments are concatenated losslessly into `sessions/session-<start>.mp4`
    ///   ([`session::finalise`]), the row names it with its real size, and the segments are removed.
    ///
    /// # When the concatenation fails
    ///
    /// A session needs transiently about twice its own bytes (the file is written in full before the
    /// segments go), so a volume that filled during recording cannot finish it. Then:
    ///
    /// * every segment is kept;
    /// * the partial output is removed (it is not a session and holds the space a retry needs);
    /// * the row is left running (`ended_at IS NULL`) on purpose: a running session is never an
    ///   eviction candidate, so retention cannot delete the only copy, and the next start's recovery
    ///   pass ([`session::recover_sessions`]) finishes the job once there is room;
    /// * the error is returned, so `stop()` and a CLI's exit code report it.
    fn finish_session(&mut self) -> Result<()> {
        match self.mode {
            RecordingMode::ReplayBuffer if self.session_dir.is_some() => {
                self.finish_saved_session()
            }
            RecordingMode::ReplayBuffer => {
                let stats = self.ledger.stats()?;
                // `size_bytes` stays 0: it is a disk claim and a RAM buffer occupies none. The held bytes go to the log.
                self.store
                    .end_session(self.session, now_ms(), None, 0, 0)
                    .with_context(|| format!("closing session #{}", self.session))?;
                tracing::info!(
                    "buffer session #{} closed: {} segment(s), {} bytes held in RAM, \
                     no session file and nothing written while buffering",
                    self.session,
                    stats.segments,
                    self.ledger.buffered_bytes()?
                );
                Ok(())
            }
            RecordingMode::FullSession => {
                // The flushed encoder's child has exited, so the newest segment is complete and belongs in the
                // session (the live scan does not trust it while ffmpeg writes it).
                self.ledger.scan_all()?;
                let (segments, directory, bin) = match &self.ledger {
                    Ledger::Session(session) => (
                        session.segments().to_vec(),
                        session.dir().to_path_buf(),
                        session.bin().clone(),
                    ),
                    Ledger::Memory(_) => bail!(
                        "a full-session recording has no session segment store: this is a \
                         bug in the recorder (mode and ledger disagree)"
                    ),
                };
                let out = session::session_file_path(&self.sessions_dir, self.session_started_ms);
                let bytes: u64 = segments.iter().map(|s| s.bytes).sum();
                if segments.is_empty() {
                    // A recording that never produced a segment (a start that failed after the row opened, or a game
                    // that ended inside ffmpeg's start-up): closed with no file to name.
                    self.store
                        .end_session(self.session, now_ms(), None, 0, 0)
                        .with_context(|| format!("closing empty session #{}", self.session))?;
                    session::remove_session_dir(&directory);
                    tracing::info!(
                        "session #{} recorded no segments; closed with no file",
                        self.session
                    );
                    return Ok(());
                }

                let meta =
                    session::finalise(&bin, &out, &segments, &self.encoder_name, self.microphone)
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
                        // The probed length of the written file: the recorded media, not the window it was recorded in.
                        meta.duration_ms as i64,
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

    /// Close the row of a replay buffer that was also saving its session: remux the file written beside the ring
    /// ([`session::finalise_part`]), name it on the row and remove the directory.
    ///
    /// A remux that fails leaves the row running and the part in place, like a failed concatenation, so the
    /// next start's recovery finishes it. A file that stopped growing early (a disk that could not keep up) is
    /// still finalised, with a warning: what it holds is real footage.
    fn finish_saved_session(&mut self) -> Result<()> {
        let dir = self
            .session_dir
            .clone()
            .context("a saved session has no directory: this is a bug in the recorder")?;
        let bin = match &self.ledger {
            Ledger::Memory(ring) => ring.bin().clone(),
            Ledger::Session(_) => bail!(
                "a replay buffer saving its session has a segment store: this is a bug in the \
                 recorder (mode and ledger disagree)"
            ),
        };
        if let Ledger::Memory(ring) = &self.ledger {
            if let Some(why) = ring.session_file_failure() {
                tracing::warn!(
                    "session #{} ends early: its file stopped growing ({why})",
                    self.session
                );
            }
        }
        let part = dir.join(memory_ring::SESSION_PART);
        let out = session::session_file_path(&self.sessions_dir, self.session_started_ms);
        let meta = session::finalise_part(&bin, &part, &out, &self.encoder_name, self.microphone)
            .with_context(|| {
            format!(
                "finalising the saved session #{} in {}",
                self.session,
                dir.display()
            )
        })?;
        match meta {
            None => {
                self.store
                    .end_session(self.session, now_ms(), None, 0, 0)
                    .with_context(|| format!("closing empty session #{}", self.session))?;
                session::remove_session_dir(&dir);
                tracing::info!(
                    "session #{} recorded nothing; closed with no file",
                    self.session
                );
            }
            Some(meta) => {
                self.store
                    .end_session(
                        self.session,
                        now_ms(),
                        Some(&meta.path.display().to_string()),
                        meta.size_bytes as i64,
                        meta.duration_ms as i64,
                    )
                    .with_context(|| format!("closing session #{}", self.session))?;
                session::remove_session_dir(&dir);
                tracing::info!(
                    "session #{} saved: {} ({}ms, {} bytes), written beside the replay buffer",
                    self.session,
                    meta.path.display(),
                    meta.duration_ms,
                    meta.size_bytes
                );
            }
        }
        Ok(())
    }

    /// Record a failure and let the loop end. Logged and kept in the status for front-ends that only poll `status()`.
    fn fail(&mut self, err: anyhow::Error) {
        let message = format!("{err:#}");
        tracing::error!("the recorder stopped: {message}");
        *self
            .status
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message);
    }
}

/// Resolve the encoder's codec and vendor, proving this machine can encode, before any capture
/// backend is created.
///
/// On Windows, creating the video backend starts capturing the user's screen. The vendor is only
/// known to work after `select_vendor` drives a one-frame smoke test, which catches advertised but
/// unusable vendors (e.g. `h264_amf` with no AMD driver, otherwise dying mid-capture with "video
/// writer thread has stopped"). Running it first makes that a fast startup failure that never touches
/// the display; the smoke test encodes its own synthetic frame, so it needs no capture size.
///
/// The size-dependent half needs the backend's `native_size`, so it stays in [`build_encode_config`].
/// The returned vendor is `None` for `dev_software_encoder` (libx264), which needs no GPU vendor.
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

/// Log the rate decision; this line is a deliverable, not a debug aid.
///
/// It turns "capture runs at 24fps" into a measured fact about this machine at this resolution.
/// It is not the timeline guarantee: the media timeline is the frames' arrival timestamps and does
/// not depend on this measurement (see `replayed_encoder::throughput`). Adapting saves the capture
/// work of frames that would be dropped and warns before recording rather than after. The
/// effective rate is printed, never the configured one when they differ.
///
/// * Reduced (`WARN`): says what was measured, what the pipeline runs at, how to get a higher
///   rate (smaller output or lower fps) and the `adapt_fps = false` escape hatch with its cost
///   (dropped frames hold the previous picture).
/// * Not reduced (`INFO`): one short line showing the measurement happened.
/// * Not measured (`INFO`): adaptation is off; the absence of a measurement is itself a decision.
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

    // The size comes from the measurement, not the config: when output is scaled, both sizes (frames fed and encoded) were measured.
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

/// The video codec the config asks for; machine-independent, so resolved before any capture backend exists.
pub fn parse_codec(spec: &str) -> Result<VideoCodec> {
    match spec {
        "h264" => Ok(VideoCodec::H264),
        "hevc" => Ok(VideoCodec::Hevc),
        other => bail!("unsupported encode.codec: {other}"),
    }
}

/// `"1920x1080"`, or `None` for the empty string (native capture resolution).
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

/// Whether this recording carries the game-audio track.
///
/// `[audio] enabled` is read only by the CLI; absent (the desktop shell) means `true`. One
/// function because the encoder config and the capture backends need the same answer.
impl RecorderConfig {
    pub fn game_audio(&self) -> bool {
        self.audio_enabled.unwrap_or(true)
    }
}

/// The capture and audio backends for the configured [`Sources`].
///
/// The audio backend is built only when `game_audio` is set: a video-only recording must not open
/// the loopback (on Windows that holds a WASAPI session the user asked not to have).
#[allow(clippy::type_complexity)]
fn build_sources(
    sources: Sources,
    capture_section: &CaptureSection,
    watch: &[replayed_events::process::WatchedGame],
    fps: u32,
    game_audio: bool,
) -> Result<(Box<dyn CaptureBackend>, Option<Box<dyn AudioBackend>>)> {
    match sources {
        // Real WGC/WASAPI capture on Windows, synthetic stubs elsewhere. On Windows a missing backend is a hard error, with no stub fallback.
        Sources::Platform => {
            let stub = StubConfig {
                width: STUB_CAPTURE_SIZE.0,
                height: STUB_CAPTURE_SIZE.1,
                fps,
            };
            let capture = default_video_backend(stub, &capture_section.target())?;
            let audio: Option<Box<dyn AudioBackend>> = if game_audio {
                // The same snapshot the game watcher takes, so the audio follows the game it detects.
                let watch = watch.to_vec();
                let find_game = Box::new(move || {
                    replayed_events::process::running_game_pid(&watch).unwrap_or_else(|e| {
                        tracing::debug!(error = %e, "could not look for the game process");
                        None
                    })
                });
                Some(game_audio_backend(
                    AudioFormat::default(),
                    capture_section.game_audio,
                    find_game,
                    &capture_section.output_device,
                )?)
            } else {
                None
            };
            Ok((capture, audio))
        }
        // Explicitly the synthetic pair, for tests or a front-end that says so; touches neither display nor speakers.
        Sources::Stub(cfg) => {
            let audio: Option<Box<dyn AudioBackend>> = if game_audio {
                Some(Box::new(StubAudio::new(AudioFormat::default())))
            } else {
                None
            };
            Ok((Box::new(StubCapture::new(cfg)), audio))
        }
    }
}

/// Build the encoder's configuration from the resolved codec and vendor plus the capture
/// backend's frame geometry, without starting the encoder. Hardware is the only shipping path.
///
/// `None` vendor is the `dev_software_encoder` (libx264) path. `native_size` (the monitor for WGC,
/// the configured size for the stub) declares the rawvideo pipe, so this runs after the backend
/// exists. Empty `encode.output_size` means native resolution; any other value is scaled from it.
///
/// Separate from [`spawn_encoder`] because the segment numbering is decided from the scratch
/// directory before the child starts (it is an ffmpeg argument and cannot change afterwards).
fn build_encode_config(
    cfg: &RecorderConfig,
    scratch_dir: &Path,
    codec: VideoCodec,
    vendor: Option<Vendor>,
    native_size: (u32, u32),
) -> Result<EncodeConfig> {
    let output_size = parse_output_size(&cfg.encode.output_size)?.unwrap_or(native_size);
    let segment_ms = cfg.buffer.segment_time.saturating_mul(1000);

    // Mutable only on Windows, where the hybrid sets `video`; the guard is unconditional so a configuration this platform cannot honour is refused here, not at capture time.
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut encode_cfg = match vendor {
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
        // `None` only for `dev_software_encoder`, reachable only from a build with the software encoder (see [`resolve_encoder`]).
        None => {
            #[cfg(any(test, feature = "test-encoders"))]
            {
                // Names the CLI's flag, the only way to reach this path (there is no config setting for it); kept verbatim from the CLI's log.
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
                // Scales like the hardware encoder: frames arrive at native size, output is `output_size`.
                c.output_size = output_size;
                c
            }
            #[cfg(not(any(test, feature = "test-encoders")))]
            {
                bail!("--dev-software-encoder requires building with `--features test-encoders`");
            }
        }
    };

    // The game track's bitrate: `[audio] bitrate_kbps` (CLI) or the default (desktop shell). Set on
    // every path so the file records what the config says.
    encode_cfg.audio_bitrate_kbps = cfg
        .audio_bitrate_kbps
        .unwrap_or(replayed_encoder::DEFAULT_AUDIO_BITRATE_KBPS);

    // Whether the game-audio track exists, from the same rule the capture backends used
    // (`RecorderConfig::game_audio`): `false` declares no audio input and no loopback listener.
    encode_cfg.game_audio = cfg.game_audio();

    // The hybrid, when asked for and possible. Both refusals are configuration contradictions caught
    // here, because a recording that silently ran a different pipeline than configured is worse than
    // one that does not start.
    // `Some(true)` is a deliberate request that requires the hardware path; `None` is "use it if you
    // can", which on a host without the hardware means the ordinary path rather than a refusal.
    let zero_copy_required = cfg.encode.zero_copy == Some(true);
    // `auto` asks the hardware, not the registry: `MFTEnumEx` says which encoders exist, not whether
    // one opens for this codec at this size (an HEVC MFT enumerates, activates, then refuses the
    // output type at 4K with `0xC00D6D76` while accepting 640x480). Choosing on enumeration alone would
    // record no video.
    #[cfg(windows)]
    let openable = vendor.is_some()
        && replayed_encoder::mft::can_encode_with_textures(
            codec,
            native_size,
            cfg.encode.bitrate_kbps,
            cfg.encode.fps,
        );
    #[cfg(not(windows))]
    let openable = false;

    let zero_copy = match cfg.encode.zero_copy {
        Some(explicit) => explicit,
        None => openable,
    };

    if zero_copy_required && vendor.is_none() {
        bail!(
            "encode.zero_copy is set, and it needs a hardware encoder: the whole point is to hand \
             the captured texture to the encoder's own MFT, and a software encoder has no MFT to \
             hand it to. Set `vendor`, unset the key to let it decide, or turn it off."
        );
    }
    #[cfg(not(windows))]
    if zero_copy_required {
        bail!(
            "encode.zero_copy is set, and it is a Windows path: the texture handover is Media \
             Foundation's encoder MFT, and there is no equivalent here. Unset the key to let it \
             decide, or turn it off."
        );
    }

    // An explicit request the hardware cannot honour is an error, not a quiet downgrade.
    #[cfg(windows)]
    if zero_copy_required && !openable {
        bail!(
            "encode.zero_copy is set, and no hardware encoder here will open for {codec:?} at \
             {}x{}: the encoders this machine lists do not include one that accepts that codec at \
             that size. Unset the key to let it decide, or turn it off.",
            native_size.0,
            native_size.1
        );
    }

    if zero_copy {
        #[cfg(windows)]
        {
            encode_cfg.video = replayed_encoder::VideoInput::EncodedBitstream;
            // Said out loud because it changes what the child does with video: it copies rather than encodes,
            // and the keyframe interval is the MFT's, not ffmpeg's.
            tracing::info!(
                "zero-copy: captured textures go straight to the hardware encoder, and ffmpeg \
                 copies the {} stream instead of encoding it ({})",
                codec.bitstream_format(),
                if zero_copy_required {
                    "set explicitly"
                } else {
                    "chosen by default, because a hardware encoder is present"
                }
            );
        }
    } else if cfg.encode.zero_copy.is_none() {
        // The default on a machine that cannot run it; said out loud so the configured and running pipelines cannot silently drift apart.
        tracing::info!(
            "zero-copy is off for this run: it needs Windows, a detected hardware encoder, and one \
             that will open for {codec:?} at this size. One of those is missing, so frames are read \
             back and encoded as usual; `encode.zero_copy = true` would make that combination a \
             startup error instead."
        );
    }

    // The rawvideo pipe is declared from `source_size`; if it differed from the backend's native size
    // ffmpeg would mis-read every frame. Guards against a future edit breaking that link.
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

/// Start the ffmpeg child for a configuration whose `start_number` is already decided (see
/// [`build_encode_config`] and the ledger's reserved segment number).
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
        if encode_cfg.source_size == encode_cfg.output_size {
            ""
        } else {
            " (scaled)"
        }
    );
    let name = encoder.active_encoder().to_string();
    Ok((Box::new(encoder), name))
}
