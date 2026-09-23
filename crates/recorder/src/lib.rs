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
//!
//! # Properties that must not regress
//!
//! Each of these was paid for once, and each has a test:
//!
//! * The trigger is expressed in the **ledger's media time** (`span_ms`), never the wall
//!   clock, and the post-roll budget is `post_ms + margin`. A wall-clock trigger made the
//!   post-roll unreachable on hardware whose media timeline runs slower than real time.
//! * The pacer decides **before** the frame is materialised, and non-due frames are
//!   drained with [`CaptureBackend::discard_pending`] so the GPU readback is skipped.
//! * The encoder is smoke-tested **before** any capture backend is created, so an
//!   unusable encoder never opens a capture session on the user's display.
//! * Clips are spliced losslessly (`-c copy`) and indexed with the row written before the
//!   file could ever be evicted (spec §8.2).
//! * The storage cleanup pass runs at startup and every [`CLEANUP_INTERVAL`].
//! * The status line (`frames= segments= bytes= span= dropped= dropped_audio= skipped=
//!   fps=`) stays observable, and `running`/counters stay readable from another thread.

pub mod config;
pub mod fps;
pub mod index;
pub mod pump;

#[cfg(test)]
mod tests;

pub use config::{BufferSection, EncodeSection, StorageSection};
pub use fps::FpsDecision;
pub use index::{
    cleanup_pass, index_clip, index_event, now_ms, open_clip_index, unix_seconds, CleanupReport,
};
pub use pump::{
    guard_frame_size, pump_once, pump_once_counted, pump_until_span, FramePacer, PumpCounts,
    RateMeter, FRAME_POLL, PACER_RESYNC_AFTER_INTERVALS, POST_ROLL_MARGIN, POST_ROLL_SCAN_INTERVAL,
    RATE_WINDOW,
};

use anyhow::{bail, Context, Result};
use localplay_capture::platform::{default_audio_backend, default_video_backend};
use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_encoder::probe::select_vendor;
use localplay_encoder::{
    EncodeConfig, Encoder, FfmpegEncoder, ThroughputMeasurement, Vendor, VideoCodec,
};
use localplay_events::{CaptureClock, GameEvent};
use localplay_media::FfmpegBinaries;
use localplay_replay::buffer::{BufferConfig, RingBuffer};
use localplay_replay::splice::ClipMetadata;
use localplay_store::Store;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
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
    /// Frames the backend offered that the pacer skipped *without* reading them back.
    pub skipped: u64,
    /// The achieved frame rate over the last [`RATE_WINDOW`]; 0.0 before one has closed.
    pub fps: f64,
    /// What `encode.fps` asked for, so the two can be shown side by side.
    pub configured_fps: u32,
    /// The rate the pipeline is actually running at — the pacer's rate and the encoder
    /// child's `-framerate`, one number. Below `configured_fps` when the startup probe
    /// measured that this machine cannot hold the configured rate and adaptation is on
    /// (startup logs that in as many words). This is the rate the media timeline is being
    /// recorded at, so it is the rate an achieved-rate readout should be compared against.
    pub effective_fps: u32,
    /// Wall clock minus media time, in ms. Positive means the media timeline is running
    /// behind real time (the measured 0.81x case), which is why the trigger is taken from
    /// `span_ms` instead.
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
    /// Commands to the loop thread. Dropping it disconnects the loop, which is how a
    /// dropped `Recorder` stops the capture even if `stop()` is never called.
    commands: Sender<Command>,
    status: Arc<SharedStatus>,
    configured_fps: u32,
    /// The rate this pipeline runs at: the pacer's interval and the encoder child's
    /// `-framerate`, from one decision ([`FpsDecision::effective`]). Equal to
    /// `configured_fps` unless the probe measured less and adaptation is on.
    effective_fps: u32,
    /// The loop thread's handle, taken by [`Recorder::stop`]. Behind a mutex so `stop`
    /// can take `&self` and stay callable from a shared `Arc<Recorder>` (which is how the
    /// desktop shell holds one) — and so a second, concurrent `stop` waits for the first
    /// rather than racing it.
    thread: Mutex<Option<JoinHandle<()>>>,
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
            error: Mutex::new(None),
        }
    }

    fn publish_rates(&self, frames: u64, skipped: u64, dropped: u64, dropped_audio: u64, fps: f64, drift_ms: i64) {
        self.frames.store(frames, Ordering::Relaxed);
        self.skipped.store(skipped, Ordering::Relaxed);
        self.dropped.store(dropped, Ordering::Relaxed);
        self.dropped_audio.store(dropped_audio, Ordering::Relaxed);
        self.fps.store(fps.to_bits(), Ordering::Relaxed);
        self.drift_ms.store(drift_ms, Ordering::Relaxed);
    }

    fn publish_ring(&self, segments: u64, bytes: u64, span_ms: u64) {
        self.segments.store(segments, Ordering::Relaxed);
        self.bytes.store(bytes, Ordering::Relaxed);
        self.span_ms.store(span_ms, Ordering::Relaxed);
    }
}

impl Recorder {
    /// Start recording.
    ///
    /// The whole startup handshake runs on this thread, in this order, because the order
    /// is the point:
    ///
    /// 1. open (and migrate) the clip index — the one step that fails hard;
    /// 2. apply the storage policy once, so a previous session's leftovers are managed now;
    /// 3. resolve the encoder's vendor with a one-frame smoke test, **before** any capture
    ///    backend exists, so an unusable encoder never opens a session on the display;
    /// 4. create the capture backend and read its native size;
    /// 5. build the encoder configuration from that size, **measure what the machine can
    ///    sustain at that size** and decide the rate the pipeline runs at
    ///    ([`FpsDecision`]), start the ring, adopt what is already on disk, decide the
    ///    encoder's first segment number, and spawn ffmpeg;
    /// 6. start capture and audio;
    /// 7. spawn the pump loop.
    ///
    /// Nothing is captured before step 6, so the segment numbering decided in step 5
    /// cannot miss footage — and the throughput probe in step 5 runs before the capture
    /// session exists, so a probe failure costs a startup error rather than a session on
    /// the user's display.
    pub fn start(cfg: RecorderConfig) -> Result<Recorder> {
        // The shipping measurement: the selected encoder, at the capture's own size, for a
        // bounded budget. The binaries are cloned into the closure so the probe does not
        // borrow `cfg` while it is being moved into `start_with_measure`.
        let bin = cfg.bin.clone();
        Self::start_with_measure(cfg, &move |encode_cfg| {
            localplay_encoder::throughput::measure_sustainable_fps(
                &bin,
                encode_cfg,
                localplay_encoder::throughput::PROBE_BUDGET,
            )
        })
    }

    /// [`Recorder::start`], with the throughput measurement injected.
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
    fn start_with_measure(
        cfg: RecorderConfig,
        measure: &dyn Fn(&EncodeConfig) -> Result<ThroughputMeasurement>,
    ) -> Result<Recorder> {
        let scratch_dir = cfg.scratch_dir();
        let clips_dir = cfg.clips_dir();

        let store = open_clip_index(&cfg.db_path())?;
        let mut cleanup = CleanupReport::default();
        cleanup_pass(&store, &cfg.storage, &mut cleanup);

        let (codec, vendor) = resolve_encoder(&cfg.bin, &cfg.encode, cfg.dev_software_encoder)?;

        // Only now create the capture backend: the encoder's rawvideo pipe is declared
        // from the backend's own frame geometry (`native_size` — the monitor under WGC,
        // the configured size for the stub), so the size-dependent half of the encoder
        // config has to wait until the backend exists.
        let (mut capture, mut audio) = build_sources(cfg.sources, cfg.encode.fps)?;
        let native = capture.native_size();

        let mut encode_cfg = build_encode_config(&cfg, &scratch_dir, codec, vendor, native)?;

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

        // The ring is built and adopted *before* the encoder is spawned because the
        // segment number the encoder must continue from is decided from what is already
        // on disk, and that number is one of ffmpeg's arguments.
        let buffer_cfg = BufferConfig {
            pre_ms: cfg.buffer.pre_seconds * 1000,
            post_ms: cfg.buffer.post_seconds * 1000,
            scratch_cap_bytes: cfg.buffer.scratch_cap_bytes,
            segment_ms: cfg.buffer.segment_time * 1000,
            clips_dir: clips_dir.clone(),
        };
        let mut ring = RingBuffer::start(
            &cfg.bin,
            buffer_cfg.clone(),
            scratch_dir.clone(),
            encode_cfg.encoder_name().to_string(),
        )?;
        let adopted = ring.adopt_existing()?;
        if adopted > 0 {
            tracing::info!("adopted {adopted} segments from a previous run");
        }
        encode_cfg.start_number = ring.reserve_segment_number()?;
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
        // rearranges the code around it, and the published rate (`effective_fps` below) is
        // then the child's own number rather than a belief about it. Read here, before the
        // encoder is moved into the engine.
        let encoder_fps = encoder.input_fps();
        // One-line capture-geometry summary so a reader can see the resolution being
        // captured and that the frame counter starts from zero (criterion 1). The rate is
        // the one the pipeline is running at; when it is below the configured rate, the
        // adaptation line above says so and why.
        tracing::info!(
            "capture geometry {}x{} at {}fps (frame counter starts at 0)",
            native.0,
            native.1,
            encoder_fps
        );

        capture.start()?;
        audio.start()?;

        let status = Arc::new(SharedStatus::new());
        let (commands, inbox) = mpsc::channel();
        let engine = Engine {
            pre_ms: buffer_cfg.pre_ms,
            post_ms: buffer_cfg.post_ms,
            scratch_cap_bytes: buffer_cfg.scratch_cap_bytes,
            storage: cfg.storage.clone(),
            cleanup,
            // The pacer paces to the rate the encoder child was told — read back from the
            // encoder itself, so the two cannot disagree. See this function's doc comment:
            // that agreement is the whole fix.
            pacer: FramePacer::new(encoder_fps),
            capture,
            audio,
            encoder,
            ring,
            store,
            clock: CaptureClock::new(),
            status: Arc::clone(&status),
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
        status.running.store(true, Ordering::SeqCst);

        let thread = std::thread::Builder::new()
            .name("localplay-recorder".to_string())
            .spawn(move || engine.run(inbox))
            .context("spawning the recording thread")?;

        Ok(Recorder {
            commands,
            status,
            configured_fps: decision.configured(),
            effective_fps: encoder_fps,
            thread: Mutex::new(Some(thread)),
        })
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
            skipped: status.skipped.load(Ordering::Relaxed),
            fps: f64::from_bits(status.fps.load(Ordering::Relaxed)),
            configured_fps: self.configured_fps,
            effective_fps: self.effective_fps,
            drift_ms: status.drift_ms.load(Ordering::Relaxed),
            clips: status.clips.load(Ordering::Relaxed),
            error,
        }
    }

    /// Take a clip now: wait for the post-roll, splice it losslessly, index it.
    ///
    /// The trigger instant is the **ledger's** media time, taken inside the recording
    /// loop at the moment the trigger is handled — not a wall-clock instant sampled here.
    /// That is the hard-won part: on hardware whose media timeline runs slower than real
    /// time (measured 0.81x) a wall-clock trigger sits beyond anything the ledger can
    /// reach, and every press times out.
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
        self.commands
            .send(Command::Clip { reason, reply })
            .map_err(|_| anyhow::anyhow!("the recorder is not running"))?;
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
        self.commands
            .send(Command::Note { event, reply })
            .map_err(|_| anyhow::anyhow!("the recorder is not running"))?;
        answer
            .recv()
            .map_err(|_| anyhow::anyhow!("the recorder stopped before it could note the event"))?
    }

    /// Stop recording: flush the encoder, close the capture and audio sources, join the
    /// loop.
    ///
    /// Idempotent — a second call is a no-op that reports the same outcome — and safe
    /// from a shared `Arc<Recorder>`, because a stop that runs concurrently with another
    /// waits for the shutdown to finish rather than returning early.
    ///
    /// Returns the failure the loop stopped for, if it stopped for one.
    pub fn stop(&self) -> Result<()> {
        // The guard is held across the join, so two concurrent stops cannot both see the
        // handle and one of them return before the thread is gone.
        let mut guard = self.thread.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(thread) = guard.take() {
            // A disconnected loop has already stopped; `send` failing is not an error.
            let _ = self.commands.send(Command::Stop);
            if thread.join().is_err() {
                return Err(anyhow::anyhow!("the recording thread panicked"));
            }
        }
        let status = self.status();
        status.error.map_or(Ok(()), |why| Err(anyhow::anyhow!(why)))
    }

    /// Whether the loop thread is still running — a cheap check for a driver's loop.
    pub fn is_running(&self) -> bool {
        self.status.running.load(Ordering::Relaxed)
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

/// The capture loop's state: the ring, the encoder, the pacer, and the counters.
struct Engine {
    pre_ms: u64,
    post_ms: u64,
    scratch_cap_bytes: u64,
    storage: StorageSection,
    cleanup: CleanupReport,
    pacer: FramePacer,
    capture: Box<dyn CaptureBackend>,
    audio: Box<dyn AudioBackend>,
    encoder: Box<dyn Encoder>,
    ring: RingBuffer,
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
    /// child's `-framerate`. The status line's denominator is this value, because it is the
    /// rate the media timeline is being recorded at — and the drop warning compares the
    /// achieved rate against it, because "the encoder cannot sustain the rate it was told"
    /// is the condition that warning exists for.
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

impl Engine {
    /// The pump loop. Returns when it is told to stop, when the last `Recorder` is
    /// dropped, or when the capture/encode path fails.
    fn run(mut self, commands: Receiver<Command>) {
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
    }

    /// One pump: submit whatever frame and audio are due.
    fn pump(&mut self) -> Result<()> {
        let counts = pump_once_counted(
            &mut self.pacer,
            self.capture.as_mut(),
            self.audio.as_mut(),
            self.encoder.as_mut(),
        )?;
        self.account(counts);
        Ok(())
    }

    /// The ~200ms tick: scan the ring, publish and log the status, enforce the scratch
    /// cap, warn about a starving encoder, and apply the storage policy on its own
    /// interval.
    fn tick(&mut self, last_cleanup: &mut Instant) -> Result<()> {
        self.ring.scan_once().context("scanning scratch")?;
        self.ring.save_ledger()?;

        let stats = self.ring.stats();
        self.status.publish_ring(
            stats.segments as u64,
            stats.bytes_on_disk,
            stats.span_ms,
        );
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
             skipped={} fps={:.1}/{} configured={}",
            self.frames,
            stats.segments,
            stats.bytes_on_disk,
            stats.span_ms,
            dropped,
            self.encoder.dropped_audio_blocks(),
            self.skipped,
            self.achieved,
            self.effective_fps,
            self.configured_fps
        );
        if stats.bytes_on_disk > self.scratch_cap_bytes {
            bail!(
                "scratch cap violated: {} bytes on disk exceeds {}",
                stats.bytes_on_disk,
                self.scratch_cap_bytes
            );
        }

        // A rising `dropped=` means the encoder's queue is overflowing while the pacer
        // admits at most the rate the pipeline is running at: the machine cannot encode that
        // rate *now* — after a startup measurement said it could, and after the pacer was
        // set to it. That must be loud rather than inferred, because its consequence is a
        // *timeline* one, not just a quality one. This is the backstop for a load that
        // changed since startup; when it fires, the recording's media time is again running
        // slower than the wall clock.
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
                 same {}fps. Media time will not track real time, so a pre_seconds clip will \
                 correspond to more real seconds than configured. Lower encode.fps, or set \
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
        // Consequence, stated on purpose: with media at 0.81x, the default
        // `pre_seconds = 10` of media is ~12.3s of real time, and the clip is "the last
        // 10s of captured media" rather than the last 10s of real time. That is the only
        // self-consistent meaning until the timeline divergence itself is fixed
        // (deferred). If the buffer holds less than `pre_ms` of media at the trigger,
        // `RingBuffer::trigger` already warns and splices the truncated front — that path
        // is unchanged.
        let trigger_ms = self.ring.stats().span_ms;
        // Wall-clock value, kept for telemetry only: nothing below reads it, because
        // mixing the two clocks is what made the post-roll unreachable. Logged next to
        // the media value so the divergence is observable in a soak — `drift` is wall
        // minus media and grows by ~190ms per second of capture at 0.81x.
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
        // The budget is `post_ms` + a margin, never a constant: the trigger instant is
        // "now" on the media timeline, so the wait covers the whole post-roll —
        // `post_ms` of MEDIA, which at the measured 0.81x is ~1.23x that in wall clock —
        // and a budget that did not account for that would time out on itself (see
        // `POST_ROLL_MARGIN`). The margin also absorbs the segment ffmpeg is still
        // appending to, whose length is `buffer.segment_time`.
        let need_ms = trigger_ms + self.post_ms;
        let budget = Duration::from_millis(self.post_ms) + POST_ROLL_MARGIN;
        let counts = pump_until_span(
            &mut self.pacer,
            &mut self.ring,
            self.capture.as_mut(),
            self.audio.as_mut(),
            self.encoder.as_mut(),
            need_ms,
            budget,
        )?;
        self.account(counts);

        let stem = format!("clip-{}", unix_seconds());
        let clip = self.ring.trigger(trigger_ms, &stem)?;
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
        let started_at_ms = self.ring.ledger_origin_ms() + trigger_ms.saturating_sub(self.pre_ms);
        let id = index_clip(&self.store, &clip, started_at_ms);

        // Spec §5.5: an event-triggered clip says *why* it exists. The row's `at` is the
        // trigger instant (which is `pre_ms` into the clip, not its start), so the marker
        // lands on the moment that caused it. A failed insert is reported and not fatal, for
        // the same reason a failed clip insert is not: the footage is what the user asked
        // for, and the clip file is on disk either way.
        if let Some(event) = reason.event() {
            let at_ms = self.ring.ledger_origin_ms() + trigger_ms;
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
        let at_ms = self.ring.ledger_origin_ms() + self.ring.stats().span_ms;
        index_event(&self.store, event, at_ms, None)
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
    }

    /// Wall clock minus media time, in ms.
    fn drift_ms(&self) -> i64 {
        self.clock.ms_at(Instant::now()) as i64 - self.ring.stats().span_ms as i64
    }

    /// Flush the encoder and close the sources.
    ///
    /// Every step is attempted even if an earlier one failed: a shutdown that gave up
    /// halfway would leave the display captured or ffmpeg writing.
    fn shutdown(&mut self) -> Result<()> {
        // The encoder first: the segment it is still appending to has to be closed before
        // the ring is scanned, or the last footage of the session is never indexed.
        let flushed = self.encoder.finish().context("flushing the encoder");
        let scanned = if flushed.is_ok() {
            self.ring.scan_once().context("scanning scratch at shutdown")
        } else {
            Ok(())
        };
        let ledger = self.ring.save_ledger().context("saving the ledger at shutdown");
        let audio = self.audio.stop().context("stopping audio capture");
        let capture = self.capture.stop().context("stopping screen capture");
        flushed.and(scanned).and(ledger).and(audio).and(capture)
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
/// *this* resolution — and it is the only place that names the consequence: the recorded
/// media timeline tracks real time because the rate declared is the rate measured, so a
/// configured `pre_seconds` of footage is that many real seconds.
///
/// Nothing here is printed from the *configured* value when the effective one differs: a
/// reader must never have to work out which of the two is in force.
///
/// * **Reduced** (`WARN`) — the configured rate is not achievable here. It says what was
///   measured, what the pipeline therefore runs at, why that is the honest choice, and how a
///   user who wants a higher rate can get one (a smaller output size, or a lower fps), plus
///   the escape hatch (`adapt_fps = false`) and what it costs (dropped frames and a media
///   timeline that runs slower than real time).
/// * **Not reduced** (`INFO`) — one short line, because there is nothing to warn about and a
///   user should still be able to see that the measurement happened and what it said.
/// * **Not measured at all** (`INFO`) — adaptation is off; say so, since the absence of a
///   measurement is itself a decision someone made.
fn log_rate_decision(decision: &FpsDecision, encoder: &str, native: (u32, u32)) {
    let Some(m) = decision.measurement() else {
        tracing::info!(
            "encode rate: not measured (encode.adapt_fps = false), so capture runs at the \
             configured {}fps at {}x{} whatever this machine can sustain: the encoder's queue \
             will drop any frame it cannot take, and the recorded timeline can then run slower \
             than real time",
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
             at {}fps instead — the same rate the encoder child is told — so that the media \
             timeline tracks real time and a configured pre_seconds of footage is that many \
             real seconds. The configured rate was not reached at this resolution: lower \
             encode.output_size (e.g. \"1920x1080\") or encode.fps to a rate this machine \
             holds, or set encode.adapt_fps = false to declare {}fps anyway and accept dropped \
             frames and a timeline that runs slower than real time.",
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
