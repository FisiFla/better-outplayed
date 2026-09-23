//! The localplay CLI pipeline.
//!
//! `apps/localplay-cli/src/main.rs` is a thin binary: it initialises tracing, parses
//! the subcommand and calls into this library. Everything the PoC actually *does*
//! lives here, because the parts that matter have to be reachable from an integration
//! test.
//!
//! That split is deliberate and was paid for once: the hotkey's post-roll wait used
//! to be an inline loop in `main`, it could never run on the macOS development host
//! (the global hotkey is Windows-only), and it shipped with a bug — the loop never
//! fed the encoder, so the buffer's span could not advance and every trigger timed
//! out. Code that no test can call is code that cannot be verified.

pub mod config;

use anyhow::{bail, Context, Result};
use config::Config;
use localplay_capture::platform::{default_audio_backend, default_video_backend};
use localplay_capture::stub::StubConfig;
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend, Frame};
use localplay_encoder::probe::select_vendor;
use localplay_encoder::{EncodeConfig, Encoder, FfmpegEncoder, Vendor, VideoCodec};
use localplay_events::hotkey::Hotkey;
use localplay_media::FfmpegBinaries;
use localplay_replay::buffer::{BufferConfig, RingBuffer};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The frame size the synthetic capture backend produces off Windows.
///
/// Real Windows capture (WGC) reports the primary monitor's native resolution; there
/// is no monitor to query on macOS, so the stub stands in at a fixed, representative
/// size. It is deliberately not the old 1920x1080 encoder default, so a macOS run
/// makes it plain that the rawvideo pipe is sized from the backend, not a constant.
const STUB_CAPTURE_SIZE: (u32, u32) = (1280, 720);

/// Longest a single `next_frame` call waits for a frame to become due.
///
/// The capture backends are real-time paced and return `None` when nothing is due
/// within the timeout, so this doubles as the pump's tick: a frame that is already due
/// comes back immediately, otherwise the call sleeps at most this long. It is what
/// keeps the loop off 100% CPU between frames — there is no separate `sleep` in the
/// pump, because sleeping is exactly what starves the encoder.
const FRAME_POLL: Duration = Duration::from_millis(5);

/// How often the scratch directory is re-scanned while waiting for a post-roll.
///
/// A scan walks the scratch directory and stats each new segment; nothing in it can
/// change faster than one segment per `buffer.segment_time`, so scanning on every pump
/// tick would spend I/O for no new information. 50ms matches the polling granularity
/// the verification runbook documents for criterion 3 (the clip must be written within
/// `post_seconds + 2s` of the keypress, of which this bounds part of the latency).
const POST_ROLL_SCAN_INTERVAL: Duration = Duration::from_millis(50);

/// Margin added to the post-roll when sizing the wait budget.
///
/// The wait lasts at least the *remaining* post-roll: `trigger_ms` is "now" on the
/// media timeline and `need_ms` is `post_ms` further along it, of footage ffmpeg has
/// not encoded yet. A fixed budget therefore races its own deadline — with the example
/// defaults (`post_seconds = 5`) a bare 5s allowance is spent entirely on the post-roll
/// itself, and whether the wait succeeds comes down to where inside a segment the
/// trigger happened to land. So the budget is sized from `post_ms` plus this margin,
/// which covers what the post-roll does not: the segment ffmpeg is still appending to
/// (up to `buffer.segment_time`), its finalisation, one scan interval, and the splice
/// that follows. Generous on purpose: a trigger that gives up early loses the clip the
/// user just asked for, and the wait is invisible to them.
///
/// It also carries one unit conversion: the budget is wall clock while `post_ms` is
/// media time, and on real hardware the media timeline runs slower than the wall clock
/// (measured 0.81x), so `post_ms` of media costs ~1.23x `post_ms` of waiting. This
/// margin covers that for any `post_seconds` up to ~20s.
const POST_ROLL_MARGIN: Duration = Duration::from_secs(5);

/// How far the pacer may fall behind the wall clock before it resynchronises.
///
/// Two frame intervals. Anything below one interval is ordinary scheduling jitter and
/// must be absorbed by the next frame being admitted a little early; anything above this
/// means the loop genuinely stalled (a slow scan of a full scratch directory, a GC-like
/// pause, a page fault), and catching up frame by frame from there would mean submitting
/// faster than `fps` for as long as the deficit lasts — a burst that re-creates exactly
/// the timeline skew the pacer exists to remove. See [`FramePacer::commit`].
const PACER_RESYNC_AFTER_INTERVALS: u32 = 2;

/// How long the achieved frame rate is measured over.
///
/// The debug line is emitted every ~200ms (the ring scan interval). A 200ms window holds
/// ~6 frames at the configured 30fps, so the rate it yields quantises to ±5fps and swings
/// around the configured value — useless for the one thing this number is for, which is
/// telling a reader whether the encoder is keeping up. A full second averages ~30 frames:
/// stable enough to read at a glance, and short enough that a shortfall (a slide, a
/// competing GPU load, a resolution switch) is visible within a second of starting.
const RATE_WINDOW: Duration = Duration::from_secs(1);

/// At most one "the encoder cannot sustain the configured fps" warning per this long.
///
/// The condition it reports (`dropped=` rising) is evaluated every 200ms, and a machine
/// that genuinely cannot encode at the configured rate stays in that state for the whole
/// run: warning once per log line would bury everything else in the log. The log's `debug`
/// line remains the continuous record; this is the part that must not be missable.
const DROP_WARN_INTERVAL: Duration = Duration::from_secs(10);

/// Admits at most `fps` frames per second into the encoder, and drops the rest.
///
/// A capture backend is not obliged to deliver frames at the rate the encoder was
/// configured for. On the Windows box the primary display delivered ~36fps while
/// `encode.fps` was 30, and the pump fed every one of them: ffmpeg then assigned a
/// timestamp per frame at the declared rate, so 25.4s of wall clock produced 19.0s of
/// media (`span=19000ms` against `need=28690ms`), the post-roll could never be reached
/// and every hotkey press timed out. Media time is now taken from the wall clock instead
/// (`-use_wallclock_as_timestamps`, see `localplay_encoder::ffmpeg`), and this pacer keeps
/// the two rates the same in the ordinary case so that fix is conservative rather than
/// load-bearing: the encoder is asked to encode `fps` frames per second, and it is given
/// `fps` frames per second.
///
/// Frames that arrive early are **dropped, not queued**: the buffer is a ring of already
/// encoded footage on disk, so holding surplus frames in memory would trade the project's
/// flat RAM for nothing at all — the encoder would still have to drop them later. They are
/// also **not read back**: the caller asks [`FramePacer::is_due`] *before* taking a frame
/// off the capture backend, so a frame the pacer has no slot for is closed by the backend
/// without the GPU copy that materialising it costs (see `pump_once_counted`). On the
/// measured 4K machine that was ~45% of all frames delivered.
///
/// Deliberately has no backlog: it is a rate limiter, not a scheduler. Nothing downstream
/// needs the frames it drops (each one is superseded by the next).
pub struct FramePacer {
    /// When the next frame may be submitted.
    next_due: Instant,
    /// `1 / fps`. Exact enough as a `Duration` (ns resolution), and unlike an accumulator
    /// of `f64` seconds it cannot drift.
    interval: Duration,
    /// Deficit beyond which `commit` resynchronises instead of catching up.
    resync_after: Duration,
}

impl FramePacer {
    /// A pacer for `fps` frames per second, with the first frame due immediately.
    ///
    /// `fps` is clamped to at least 1: the same value drives the encoder's arguments and
    /// `-rate`-style arithmetic, and a zero would be a division by zero rather than a
    /// meaningful "no limit".
    pub fn new(fps: u32) -> Self {
        let fps = fps.max(1);
        let interval = Duration::from_nanos(1_000_000_000 / u64::from(fps));
        Self {
            next_due: Instant::now(),
            interval,
            resync_after: interval * PACER_RESYNC_AFTER_INTERVALS,
        }
    }

    /// Whether a frame is due at `now` — i.e. whether the caller should take one off the
    /// capture backend at all.
    ///
    /// `false` means *do not take a frame*: nothing may be submitted yet, and the frames
    /// the backend is holding are surplus. Asking this **before** the frame exists is what
    /// lets the caller skip the readback for the frames it will not keep; asking it (as
    /// this used to be written, as [`FramePacer::admit`]) after the frame had already been
    /// materialised meant paying for the copy and then throwing it away.
    ///
    /// This is a query with no side effects: a slot is consumed by
    /// [`FramePacer::commit`], and only once a frame has actually been obtained. That is
    /// what keeps the long-run rate honest at a *maximum* of `fps` (nothing can be
    /// committed twice for one slot) and at *exactly* `fps` when frames are available: the
    /// schedule advances by one interval per submitted frame, so an interval in which the
    /// backend produced nothing is not silently skipped.
    pub fn is_due(&self, now: Instant) -> bool {
        now >= self.next_due
    }

    /// Advance the schedule by one interval, because a frame was obtained and submitted.
    ///
    /// Must be called at most once per frame, and only after [`FramePacer::is_due`]
    /// returned `true`. It is deliberately unconditional: by the time a frame exists the
    /// decision has been made, and re-deciding here (the old `admit` did, from the time
    /// the frame arrived) would mean dropping a frame that had already been read back.
    ///
    /// The schedule advances by exactly one interval per committed frame, so the long-run
    /// rate is `fps` with no accumulating drift from the time each call happens to be made.
    /// The exception is the resync below, and it exists so that a stall cannot turn into a
    /// burst: when the deficit is [`PACER_RESYNC_AFTER_INTERVALS`] intervals or more, this
    /// frame is admitted but the schedule jumps to `now + interval` rather than staying in
    /// the past. Catching up instead would submit as fast as the capture backend hands
    /// frames over until the deficit was paid off, which is the timeline skew this type
    /// exists to prevent (and, at the encoder's bounded queue, would mostly be dropped
    /// there instead — see `localplay_encoder::ffmpeg`).
    pub fn commit(&mut self, now: Instant) {
        self.next_due += self.interval;
        if now.saturating_duration_since(self.next_due) >= self.resync_after {
            self.next_due = now + self.interval;
        }
    }

    /// How long until the next slot, `Duration::ZERO` if one is already due.
    ///
    /// The pump uses this to wait without spinning while it is not due: `discard_pending`
    /// never blocks, unlike `next_frame`, so without a wait the pump's loop would run hot
    /// for the whole interval — the exact cost this pacer's discard path exists to remove.
    pub fn time_until_due(&self, now: Instant) -> Duration {
        self.next_due.saturating_duration_since(now)
    }
}

/// The rate frames are actually reaching the encoder at, averaged over `window`.
///
/// This is the number that decides whether media time tracks real time (the difference
/// between a clip's configured `pre_seconds` and the real seconds it covers), so it is
/// worth measuring honestly rather than inferring from the slope of `frames=` in a log:
/// it counts the frames that got past the encoder's queue, not the ones the pacer admitted
/// (the two differ exactly when the machine cannot encode at the configured rate — the
/// case this is here to expose) and not the ones the source offered (which the pacer
/// discards on purpose, see [`CaptureBackend::discard_pending`]).
///
/// `record` is called on every pump with the frames that survived since the previous call,
/// so the measurement covers the whole run rather than whichever instant a log line
/// happened to sample. The reported value is the last *completed* window, which is why a
/// fresh meter reports 0.0: the first second of a run has no measured window yet. That is
/// deliberate — a rate computed over the first 200ms of a run is not a rate, it is noise.
#[derive(Debug)]
struct RateMeter {
    /// How long a window lasts; it ends at the first `record` after this much time.
    window: Duration,
    /// Frames recorded in the window that is currently open.
    frames: u64,
    /// When the open window started.
    since: Instant,
    /// The rate measured over the last *completed* window; 0.0 before the first one.
    fps: f64,
}

impl RateMeter {
    fn new(window: Duration) -> Self {
        Self { window, frames: 0, since: Instant::now(), fps: 0.0 }
    }

    /// Add `frames` to the open window and return the achieved rate.
    ///
    /// `now` is a parameter rather than a fresh `Instant::now()` so the meter's arithmetic
    /// is testable without sleeping: nothing here is timing-dependent, only
    /// time-*stamped*.
    fn record(&mut self, frames: u64, now: Instant) -> f64 {
        self.frames += frames;
        let elapsed = now.saturating_duration_since(self.since);
        if elapsed >= self.window {
            // Divided by the window that actually elapsed, not by `self.window`: the
            // window closes on a pump, which is at most [`FRAME_POLL`] late, so the two
            // differ by milliseconds — but dividing by the nominal length would report a
            // rate that is slightly too high, and this number is used to decide whether
            // the machine is keeping up.
            self.fps = self.frames as f64 / elapsed.as_secs_f64();
            self.frames = 0;
            self.since = now;
        }
        self.fps
    }
}

/// The `buffer` subcommand: capture → encode → segment ring → hotkey → clip.
pub fn run_buffer() -> Result<()> {
    let app_dir = app_data_dir();
    let cfg_path = app_dir.join("config.toml");
    let cfg = if cfg_path.is_file() {
        Config::load(&cfg_path)?
    } else {
        Config::from_toml(include_str!("../../../config.example.toml"))?
    };

    let bin = FfmpegBinaries::discover(None)?;
    let hotkey = Hotkey::parse(&cfg.hotkeys.clip)?;

    let scratch_dir = if cfg.buffer.scratch_dir.is_empty() {
        app_dir.join("scratch")
    } else {
        PathBuf::from(&cfg.buffer.scratch_dir)
    };
    let clips_dir = if cfg.storage.clips_dir.is_empty() {
        app_dir.join("clips")
    } else {
        PathBuf::from(&cfg.storage.clips_dir)
    };

    let buffer_cfg = BufferConfig {
        pre_ms: cfg.buffer.pre_seconds * 1000,
        post_ms: cfg.buffer.post_seconds * 1000,
        scratch_cap_bytes: cfg.buffer.scratch_cap_bytes,
        segment_ms: cfg.buffer.segment_time * 1000,
        clips_dir: clips_dir.clone(),
    };

    // Resolve and validate the encoder — the codec and the vendor, the latter proven
    // usable by a one-frame smoke test — *before* any capture backend is created. On
    // Windows the backend call below opens a WGC capture session on the primary monitor,
    // so doing this first is what lets an unusable encoder fail fast without ever touching
    // the display (see [`resolve_encoder`]).
    //
    // `--dev-software-encoder` only exists when built with the test-encoders feature; it
    // selects libx264, needs no GPU vendor at all, and so is never gated behind a hardware
    // check.
    let dev_software = std::env::args().any(|a| a == "--dev-software-encoder");
    let (codec, vendor) = resolve_encoder(&bin, &cfg, dev_software)?;

    // Only now create the capture backend: the encoder's rawvideo pipe is declared from the
    // backend's own frame geometry (`native_size` — the monitor under WGC, the configured
    // size for the stub), so the size-dependent half of the encoder config has to wait
    // until the backend exists.
    let mut capture = default_video_backend(StubConfig {
        width: STUB_CAPTURE_SIZE.0,
        height: STUB_CAPTURE_SIZE.1,
        fps: cfg.encode.fps,
    })?;
    let native = capture.native_size();

    let mut encode_cfg = build_encode_config(&cfg, &scratch_dir, codec, vendor, native)?;

    // The backend is chosen per platform: real WGC/WASAPI capture on Windows, the
    // synthetic stubs elsewhere. On Windows a missing capture backend is a hard error
    // there is no stub fallback to hide it behind.
    //
    // The ring is built and adopted *before* the encoder is spawned (where this used to
    // be the other way round) because the segment number the encoder must continue from
    // is decided from what is already on disk, and that number is one of ffmpeg's
    // arguments. Nothing is captured in between: the capture backend has not been
    // started yet, so no frame can be missed by the reordering.
    let mut ring = RingBuffer::start(
        &bin,
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

    let (mut encoder, encoder_name) = spawn_encoder(&bin, &encode_cfg)?;
    tracing::info!("encoding with {encoder_name}");
    // One-line capture-geometry summary so a reader can see the resolution being
    // captured and that the frame counter starts from zero (criterion 1).
    tracing::info!(
        "capture geometry {}x{} at {}fps (frame counter starts at 0)",
        native.0,
        native.1,
        cfg.encode.fps
    );

    let mut audio = default_audio_backend(AudioFormat::default())?;
    capture.start()?;
    audio.start()?;

    let hotkeys = localplay_events::hotkey::listen(hotkey)?;
    let clock = localplay_events::CaptureClock::new();
    // One pacer for every path that pumps: the steady-state loop and the post-roll wait
    // share the encoder's timeline, so they have to share its rate limit too.
    let mut pacer = FramePacer::new(cfg.encode.fps);
    tracing::info!(
        "buffering {}s pre / {}s post at {}fps; press {} to clip",
        cfg.buffer.pre_seconds,
        cfg.buffer.post_seconds,
        cfg.encode.fps,
        cfg.hotkeys.clip
    );

    // Capture loop. Frames go to the encoder; the ring scans for completed segments.
    let mut last_scan = Instant::now();
    // Frames this loop submitted to the encoder (criterion 1's `frames=` counter, "a
    // literal counter of the video frames actually submitted to the encoder" — runbook
    // §1). Frames submitted by a post-roll wait are counted by `pump_until_span`, which
    // logs its own total rather than folding it in here.
    let mut frames: u64 = 0;
    // Frames the backend offered and the pacer threw away *without* the readback
    // (`CaptureBackend::discard_pending`). Logged next to `frames=` because the two
    // together are the whole accounting of what the source produced, and because a
    // non-zero `skipped=` next to a `frames=` holding the configured rate is the
    // signature of this optimisation working: the surplus is being skipped cheaply
    // instead of being copied and then dropped.
    let mut skipped: u64 = 0;
    // The achieved rate, the encoder-drop accounting behind it, and the state of the
    // "the encoder cannot keep up" warning. All three are fed on every pump rather than
    // sampled at log time.
    let mut rate = RateMeter::new(RATE_WINDOW);
    // Encoder drops already subtracted from the rate meter. Each dropped frame is taken
    // out of the achieved rate exactly once — when it first becomes visible in
    // `Encoder::dropped_frames`, which is cumulative.
    let mut dropped_counted: u64 = 0;
    // The dropped count as of the last warning, so "still dropping" can be told from "was
    // dropping once, an hour ago".
    let mut last_warned_dropped: u64 = 0;
    let mut last_drop_warning: Option<Instant> = None;
    loop {
        let counts =
            pump_once_counted(&mut pacer, capture.as_mut(), audio.as_mut(), encoder.as_mut())?;
        frames += counts.submitted;
        skipped += counts.skipped;
        // Frames that reached ffmpeg: everything handed to the encoder, minus whatever its
        // bounded queue dropped because it could not keep up. This, not `frames=`, is "the
        // achieved rate" — it is the rate the recorded footage actually carries.
        let dropped = encoder.dropped_frames();
        let reached = counts.submitted.saturating_sub(dropped.saturating_sub(dropped_counted));
        dropped_counted = dropped;
        let achieved = rate.record(reached, Instant::now());

        if last_scan.elapsed() >= Duration::from_millis(200) {
            ring.scan_once().context("scanning scratch")?;
            ring.save_ledger()?;
            last_scan = Instant::now();

            let stats = ring.stats();
            // `dropped=` is the encoder's own count of frames it had to discard because
            // its queue was full (it cannot slow the capture down, see the queue note in
            // `localplay-encoder`). It belongs next to `frames=` because the two together
            // say whether the pipeline is keeping up: a run that quietly loses a third of
            // its frames must not look like a clean one. `dropped_audio=` is the same
            // count for audio blocks; it is logged separately because a drop there is a
            // hole in the sound rather than a repeated picture, and the two have different
            // acceptable rates.
            //
            // `skipped=` and `frames=` are one account: every frame the backend offered was
            // either submitted to the encoder or skipped without the readback, so
            // `frames=` + `skipped=` is the source's own rate. `fps=` is the achieved rate
            // over the last [`RATE_WINDOW`] — the frames that actually reached the encoder
            // per second — printed against the configured rate so a shortfall is readable
            // at a glance instead of inferred from a counter's slope.
            tracing::debug!(
                "frames={} segments={} bytes={} span={}ms dropped={} dropped_audio={} \
                 skipped={} fps={:.1}/{}",
                frames,
                stats.segments,
                stats.bytes_on_disk,
                stats.span_ms,
                dropped,
                encoder.dropped_audio_blocks(),
                skipped,
                achieved,
                cfg.encode.fps
            );
            if stats.bytes_on_disk > cfg.buffer.scratch_cap_bytes {
                bail!(
                    "scratch cap violated: {} bytes on disk exceeds {}",
                    stats.bytes_on_disk,
                    cfg.buffer.scratch_cap_bytes
                );
            }

            // A rising `dropped=` means the encoder's queue is overflowing while the pacer
            // admits at most the configured rate: the machine cannot encode that rate.
            // That must be loud rather than inferred, because its consequence is a
            // *timeline* one, not just a quality one.
            let dropped_since_last = dropped.saturating_sub(last_warned_dropped);
            let warn_due = last_drop_warning.is_none_or(|t| t.elapsed() >= DROP_WARN_INTERVAL);
            // `achieved > 0.0` means a rate has been measured at all: before the first
            // window closes there is no number to quote, so the warning waits for one.
            // Nothing is lost by waiting — `dropped_since_last` stays non-zero until a
            // warning is actually emitted, which is also what keeps this from being
            // printed once per log line.
            if dropped_since_last > 0 && warn_due && achieved > 0.0 {
                tracing::warn!(
                    "the encoder cannot sustain the configured {}fps: only {achieved:.1} \
                     frames per second are reaching it, and its queue is dropping the rest \
                     ({dropped_since_last} since the last report, {dropped} in total) even \
                     though the pacer admits at most the configured {}fps. Media time will \
                     not track real time, so a pre_seconds clip will correspond to more \
                     real seconds than configured. Lower encode.fps, or set \
                     encode.output_size smaller.",
                    cfg.encode.fps,
                    cfg.encode.fps
                );
                last_drop_warning = Some(Instant::now());
                last_warned_dropped = dropped;
            }
        }

        if localplay_events::hotkey::wait_for_press(&hotkeys, Duration::from_millis(10)) {
            // The trigger is "now" on the LEDGER's timeline — media time — not on the
            // wall clock, and that is deliberate: the two clocks only agree while the
            // pipeline keeps up with `encode.fps`, and on real 4K hardware it does not.
            // Measured on the box (RTX 3090, 3840x2160): segments were written at 0.81/s
            // while each one contained exactly 1.000000s of media, so the media timeline
            // advanced at ~0.81x of the wall clock. A wall-clock `trigger_ms` made
            // `need_ms` unreachable — the ledger can never catch up to a target derived
            // from a clock running ~19% ahead of it — which is exactly the measured
            // failure this replaces: `timed out ... waiting for post-roll (span=27000ms
            // need=29236ms)`. `span_ms` is the end of the footage the ring can prove is
            // on disk, i.e. the media-time position of "now"; `need_ms` and the splice
            // window `[trigger_ms - pre_ms, trigger_ms + post_ms]` are then all on that
            // same clock, so the post-roll target is reachable by construction.
            //
            // Consequence, stated on purpose: with media at 0.81x, the default
            // `pre_seconds = 10` of media is ~12.3s of real time, and the clip is "the
            // last 10s of captured media" rather than the last 10s of real time. That is
            // the only self-consistent meaning until the timeline divergence itself is
            // fixed (deferred). If the buffer holds less than `pre_ms` of media at the
            // press, `RingBuffer::trigger` already warns and splices the truncated
            // front — that path is unchanged.
            let trigger_ms = ring.stats().span_ms;
            // Wall-clock value, kept for telemetry only: nothing below reads it, because
            // mixing the two clocks is what made the post-roll unreachable. Logged next
            // to the media value so the divergence is observable in a soak — `drift` is
            // wall minus media and grows by ~190ms per second of capture at 0.81x.
            let wall_ms = clock.ms_at(Instant::now());
            tracing::info!(
                "hotkey pressed: media={trigger_ms}ms wall={wall_ms}ms (drift {}ms); \
                 waiting for post-roll",
                wall_ms as i64 - trigger_ms as i64
            );

            // Wait for the post-roll to be written before splicing (spec §6.2 step 2).
            //
            // The budget is `post_ms` + a margin, never a constant: the trigger instant
            // is "now" on the media timeline, so the wait covers the whole post-roll —
            // `post_ms` of MEDIA, which at the measured 0.81x is ~1.23x that in wall
            // clock — and a budget that did not account for that would time out on
            // itself (see `POST_ROLL_MARGIN`). The margin also absorbs the segment
            // ffmpeg is still appending to, whose length is `buffer.segment_time`.
            let need_ms = trigger_ms + buffer_cfg.post_ms;
            let budget = Duration::from_millis(buffer_cfg.post_ms) + POST_ROLL_MARGIN;
            pump_until_span(
                &mut pacer,
                &mut ring,
                capture.as_mut(),
                audio.as_mut(),
                encoder.as_mut(),
                need_ms,
                budget,
            )?;

            let stem = format!("clip-{}", unix_seconds());
            let clip = ring.trigger(trigger_ms, &stem)?;
            tracing::info!(
                "wrote {} ({}ms, {} bytes, encoder={})",
                clip.path.display(),
                clip.duration_ms,
                clip.size_bytes,
                clip.encoder
            );
        }
    }
}

/// What one [`pump_once_counted`] call did with the frames the backend offered.
///
/// The two counters together account for every frame the capture backend made available:
/// `submitted` are the ones the encoder was handed, `skipped` the ones the pacer had no
/// slot for — which were closed by the backend *without* being read back. That split is
/// the measurement this pipeline was missing: a skipped frame used to cost exactly as much
/// as a submitted one (a 33.2MB staging copy at 3840x2160) and be thrown away after the
/// copy had been paid for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PumpCounts {
    /// Frames handed to the encoder.
    pub submitted: u64,
    /// Frames the backend offered that the pacer will not keep, discarded without copying
    /// their pixels to CPU memory ([`CaptureBackend::discard_pending`]).
    pub skipped: u64,
}

/// Submit every video frame and audio block that is due right now.
///
/// See [`pump_once_counted`], which is the implementation; this is the same call for
/// callers that do not need the counts.
pub fn pump_once(
    pacer: &mut FramePacer,
    capture: &mut dyn CaptureBackend,
    audio: &mut dyn AudioBackend,
    encoder: &mut dyn Encoder,
) -> Result<()> {
    pump_once_counted(pacer, capture, audio, encoder)?;
    Ok(())
}

/// [`pump_once`], reporting what became of the frames the backend offered.
///
/// Both the steady-state loop and the post-roll wait pump through here, so the rule
/// "keep the encoder fed" lives in exactly one place. The counts exist so the binary can
/// keep criterion 1's `frames=` counter while that rule stays centralised — and so the
/// frames the pacer throws away show up as `skipped=` rather than as work nobody priced.
///
/// Video, in this order, and for this reason:
///
/// 1. **Ask the pacer first** ([`FramePacer::is_due`]). When no frame is due, nothing is
///    taken off the backend: `discard_pending` closes what it is holding *without* the
///    readback, which is the entire point of the split. This used to be the other way
///    round — `next_frame` first, then the pacer — so `CopyResource` plus a row-by-row CPU
///    copy (33.2MB at 3840x2160) was paid for every frame, including the ~45% the pacer
///    dropped straight afterwards.
/// 2. When a frame *is* due, take one (`next_frame` waits at most [`FRAME_POLL`]) and then
///    commit the slot. The schedule is advanced by a frame that exists rather than by an
///    expectation, so a `next_frame` that times out leaves the pacer still due — it does
///    not skip a slot, and the long-run rate stays at the configured `fps`.
/// 3. Submit it, once `guard_frame_size` has checked it against the size the encoder's
///    rawvideo pipe was declared with.
///
/// The not-due branch waits (bounded by [`FRAME_POLL`], never past the next slot). It has
/// to: `discard_pending` never blocks, unlike `next_frame`, so a loop with no wait here
/// would spin at 100% CPU for the rest of every interval — the exact cost this change
/// exists to remove. Sleeping until the slot cannot starve the encoder (a frame the pacer
/// is not due for is a frame it will not keep), and the cap keeps the caller's other work
/// — the hotkey poll, the ring scan, the periodic log line — at the granularity it has
/// always had.
///
/// The pacer is what keeps the encoder's media timeline honest. A capture backend delivers
/// frames at its own rate, which is not necessarily `encode.fps` (on real hardware the
/// display delivered 53-75fps against a configured 30); submitting all of them asks the
/// encoder to encode a timeline that advances faster than the wall clock, and the
/// post-roll then never arrives. A frame the pacer drops is *not counted* as submitted,
/// which is the honest reading of `frames=`: it counts frames the encoder received.
///
/// Audio: drain **every** block that is already due, unpaced. Audio blocks are 10ms
/// while video frames are 16.7ms at 60fps, so submitting a single block per iteration
/// would run audio at ~60% speed and desync the clip. A zero timeout makes
/// `next_buffer` a non-blocking "is anything due?" check. Audio is not rate-limited
/// because its timeline is the exact 48kHz sample count rather than an arrival
/// timestamp: throttling it to the video rate would *create* the desync it looks like
/// it is preventing.
///
/// A frame the backend did not produce is not counted; a submit that errors aborts.
pub fn pump_once_counted(
    pacer: &mut FramePacer,
    capture: &mut dyn CaptureBackend,
    audio: &mut dyn AudioBackend,
    encoder: &mut dyn Encoder,
) -> Result<PumpCounts> {
    let mut counts = PumpCounts::default();
    if pacer.is_due(Instant::now()) {
        if let Some(frame) = capture.next_frame(FRAME_POLL)? {
            guard_frame_size(&frame, encoder.source_size())?;
            // One slot, one frame. Committing here — after the frame exists, rather than
            // at the instant it was offered — is what lets `is_due` above be a pure query.
            pacer.commit(Instant::now());
            // Ownership moves into the encoder so the pixel buffer is queued, not
            // copied: at 3840x2160 BGRA a clone is 33.2MB per frame, ~1GB/s of pure
            // memcpy at 30fps on this path (see `localplay_encoder::ffmpeg`).
            encoder.submit_video(frame)?;
            counts.submitted += 1;
        }
    } else {
        counts.skipped += capture.discard_pending()? as u64;
        // The wait described above. `time_until_due` is measured from a fresh `now`
        // because the discard call above took time of its own; it saturates to zero if the
        // slot came due in the meantime, in which case the next iteration takes the frame.
        std::thread::sleep(pacer.time_until_due(Instant::now()).min(FRAME_POLL));
    }
    while let Some(block) = audio.next_buffer(Duration::ZERO)? {
        encoder.submit_audio(block)?;
    }
    Ok(counts)
}

/// Refuse a frame whose geometry disagrees with the rawvideo pipe's declared size.
///
/// The pipe is a flat byte stream that ffmpeg slices into frames of the size it was
/// spawned with (`EncodeConfig::source_size`, i.e. [`Encoder::source_size`]); it carries
/// no framing of its own, so a frame of a different size is not rejected by anything.
/// It is *mis-read*: every boundary after the first lands mid-frame, and the picture
/// comes apart in diagonal bands while the segment files still look healthy and the
/// logs stay clean. That is a silent-corruption class of bug, and this project found
/// it the hard way: on Windows 11 a 4K desktop at 150% scaling had the rawvideo pipe
/// declared 2560x1440 (logical pixels, from a DPI-virtualised `GetSystemMetrics`) while
/// the capture item was 3840x2160 physical pixels — the size every frame carries.
///
/// The two values are derived from one another in `build_encode_config` — `native_size`
/// builds the `EncodeConfig` and this compares the frames against it — so a mismatch
/// means that link is broken (a resolution change mid-capture, or a backend reporting a
/// size it does not deliver). Either way the safe answer is to stop, loudly, rather than
/// hand ffmpeg bytes it will misread.
fn guard_frame_size(frame: &Frame, configured: (u32, u32)) -> Result<()> {
    if (frame.width, frame.height) != configured {
        bail!(
            "capture produced a {}x{} frame, but the encoder's raw video pipe was \
             declared for the configured source size {}x{}: ffmpeg reads that pipe as a \
             flat byte stream, so a frame of any other size would be mis-read (garbled \
             bands, stream desync) rather than reported",
            frame.width,
            frame.height,
            configured.0,
            configured.1
        );
    }
    Ok(())
}

/// Wait until the ring's segment span reaches `need_ms`, keeping the encoder fed.
///
/// This is the hotkey's post-roll wait (spec §6.2 step 2), and it is deliberately
/// **not** a sleep: ffmpeg only advances its segment timeline while frames keep
/// arriving on the rawvideo pipe, and the ring only advances its span when ffmpeg
/// finalises a segment. A loop that slept and re-scanned without feeding the encoder
/// would never see `span_ms` move and would always hit its own deadline — which is
/// exactly what the code this replaces did, on every trigger, since the trigger path
/// had no test that could run it (the hotkey is Windows-only).
///
/// Each iteration therefore pumps capture → encoder exactly as the main loop does,
/// through the same [`FramePacer`] the main loop uses: the two paths feed one encoder and
/// so share one rate limit, otherwise the wait would quietly submit at the raw capture
/// rate and drift the timeline it is waiting on.
///
/// `budget` bounds the wait, measured from this call. Callers size it from the
/// post-roll they are waiting for plus a margin ([`POST_ROLL_MARGIN`]) rather than from
/// a constant, because the wait itself lasts at least the remaining post-roll. Note the
/// two clocks in one call: `need_ms` and the span are media time, `budget` is wall
/// clock, and on real hardware the former advances slower than the latter (measured
/// 0.81x) — which is why the caller adds a margin rather than passing a bare `post_ms`.
///
/// Returns `Ok(())` once the span covers `need_ms`; a span already at or beyond
/// `need_ms` on entry returns immediately. Errors if the budget elapses first, or if
/// the capture/encode path fails.
pub fn pump_until_span(
    pacer: &mut FramePacer,
    ring: &mut RingBuffer,
    capture: &mut dyn CaptureBackend,
    audio: &mut dyn AudioBackend,
    encoder: &mut dyn Encoder,
    need_ms: u64,
    budget: Duration,
) -> Result<()> {
    let started = Instant::now();
    let deadline = started + budget;
    let mut frames: u64 = 0;
    // Scan on the first pass, then on the interval: the span can only move when
    // ffmpeg finalises a segment, which is a per-segment event, not a per-frame one.
    let mut next_scan = started;

    loop {
        frames += pump_once_counted(pacer, capture, audio, encoder)?.submitted;

        if Instant::now() >= next_scan {
            ring.scan_once().context("scanning scratch for the post-roll")?;
            next_scan = Instant::now() + POST_ROLL_SCAN_INTERVAL;
            let span = ring.stats().span_ms;
            if span >= need_ms {
                tracing::debug!(
                    "post-roll on disk: span={span}ms covers {need_ms}ms after {}ms \
                     ({frames} frames submitted while waiting)",
                    started.elapsed().as_millis()
                );
                return Ok(());
            }
        }

        if Instant::now() >= deadline {
            bail!(
                "timed out after {}ms waiting for post-roll (span={}ms need={}ms): the \
                 encoder produced no segment covering the trigger",
                started.elapsed().as_millis(),
                ring.stats().span_ms,
                need_ms
            );
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

/// Resolve the encoder's codec and vendor — proving this machine can actually encode —
/// **before** any capture backend is created.
///
/// WHY THIS RUNS FIRST. On Windows, creating the video backend opens a Windows Graphics
/// Capture session on the primary monitor: it starts capturing the user's screen. The
/// vendor cannot be known to work until [`select_vendor`] has driven a one-frame smoke
/// test of the candidate encoder, and that smoke test is exactly where an
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
/// The returned vendor is `None` for `--dev-software-encoder`, the libx264 smoke-test
/// path, which needs no GPU vendor at all and must not be gated behind a hardware check.
/// `dev_software` is only reachable when the CLI is built with `--features test-encoders`,
/// and is never reachable from the config file (see `Config::validate`).
fn resolve_encoder(
    bin: &FfmpegBinaries,
    cfg: &Config,
    dev_software: bool,
) -> Result<(VideoCodec, Option<Vendor>)> {
    let codec = parse_codec(&cfg.encode.codec)?;
    if dev_software {
        #[cfg(feature = "test-encoders")]
        {
            return Ok((codec, None));
        }
        #[cfg(not(feature = "test-encoders"))]
        {
            bail!("--dev-software-encoder requires building with `--features test-encoders`");
        }
    }
    Ok((codec, Some(select_vendor(bin, &cfg.encode.vendor, codec)?)))
}

/// The video codec the config asks for. Machine-independent, so it is resolved alongside
/// the vendor, before any capture backend exists.
fn parse_codec(spec: &str) -> Result<VideoCodec> {
    match spec {
        "h264" => Ok(VideoCodec::H264),
        "hevc" => Ok(VideoCodec::Hevc),
        other => bail!("unsupported encode.codec: {other}"),
    }
}

/// Build the encoder's configuration from the already-resolved codec and vendor plus the
/// capture backend's own frame geometry — without starting the encoder. Hardware is the
/// only shipping path.
///
/// The vendor is resolved earlier, before the capture backend exists (see
/// [`resolve_encoder`]); `None` here is the `--dev-software-encoder` (libx264) path.
/// `native_size` is the capture backend's own frame geometry: the monitor for WGC, the
/// configured size for the stub. The encoder's rawvideo pipe is declared from it, which is
/// why this size-dependent step has to run *after* the backend exists. Empty
/// `encode.output_size` means "encode at native resolution" (config.example.toml, spec
/// §10); any other value is the output size, scaled from the native frames.
///
/// Separate from [`spawn_encoder`] because the process has to know the encoder's name
/// (for the ring's clip metadata) and, more importantly, has to decide the segment
/// numbering from the scratch directory *before* the child starts: the number is an
/// ffmpeg argument, so it cannot be changed afterwards.
fn build_encode_config(
    cfg: &Config,
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
        // `vendor` is `None` only for `--dev-software-encoder`, which is only reachable
        // when built with `test-encoders` (see [`resolve_encoder`]).
        None => {
            #[cfg(feature = "test-encoders")]
            {
                tracing::warn!(
                    "--dev-software-encoder: using libx264. This is for smoke-testing the \
                     pipeline only and is NOT a supported configuration."
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
            #[cfg(not(feature = "test-encoders"))]
            {
                bail!("--dev-software-encoder requires building with `--features test-encoders`");
            }
        }
    };

    // The rawvideo pipe is declared from `source_size`; if it did not equal the
    // backend's native size ffmpeg would mis-read every frame. The CLI derives it from
    // `native_size`, so this guards against a future edit breaking that link.
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

/// `"1920x1080"`, or `None` for the empty string (documented as "native capture
/// resolution", config.example.toml / spec §10).
fn parse_output_size(spec: &str) -> Result<Option<(u32, u32)>> {
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

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pacer plus the instant its schedule is measured from.
    ///
    /// `FramePacer::new` stamps `next_due` from the clock itself, so the returned `t0` is a
    /// few microseconds *after* that stamp: every instant below is expressed relative to
    /// `t0`, and every margin is at least a millisecond, which is orders of magnitude
    /// wider than the difference. Nothing here sleeps — the pacer only ever reads the
    /// instant it is handed, so these tests are deterministic and instant.
    fn pacer(fps: u32) -> (FramePacer, Instant) {
        let p = FramePacer::new(fps);
        (p, Instant::now())
    }

    /// The pump's composition, minus capture and encoder: *"a frame arrived at `now`"*.
    ///
    /// Ask whether a slot is due, and spend one only if it is — exactly the video half of
    /// [`pump_once_counted`]. Testing the pacer through the shape the pump actually uses
    /// is what keeps the two from drifting apart: a pacer that answered `is_due` without
    /// ever spending the slot, or spent it without being asked, fails these tests.
    fn frame_arrived(p: &mut FramePacer, now: Instant) -> bool {
        if !p.is_due(now) {
            return false; // the pump's discard branch: no frame is taken off the backend
        }
        p.commit(now);
        true
    }

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_frame_that_is_due_is_admitted() {
        let (mut p, t0) = pacer(10);
        assert!(frame_arrived(&mut p, t0), "the first frame is due immediately");
        assert!(frame_arrived(&mut p, t0 + 100 * MS), "the frame at the next slot is due");
    }

    #[test]
    fn a_frame_that_arrives_early_is_dropped() {
        let (mut p, t0) = pacer(10);
        assert!(frame_arrived(&mut p, t0));
        // Half a frame interval early: the capture backend ran ahead of the encoder's
        // rate. This is the frame that made 36fps of capture look like 30fps of media —
        // and in the pump, "not due" now means this frame is never even read back.
        assert!(
            !frame_arrived(&mut p, t0 + 50 * MS),
            "a frame before its slot must be dropped, not queued and not submitted"
        );
        // And dropping it did not move the schedule: the slot is still at t0 + 100ms.
        assert!(!frame_arrived(&mut p, t0 + 99 * MS), "still early");
        assert!(frame_arrived(&mut p, t0 + 100 * MS), "the slot itself is admitted");
    }

    #[test]
    fn a_capture_running_faster_than_configured_is_limited_to_the_configured_rate() {
        // A 60fps capture against a 30fps configuration: half the frames must be dropped,
        // and `frames=` must not report them as submitted.
        let (mut p, t0) = pacer(30);
        let mut admitted = 0;
        for i in 0..60 {
            if frame_arrived(&mut p, t0 + Duration::from_micros(i * 1_000_000 / 60)) {
                admitted += 1;
            }
        }
        assert!(
            (29..=31).contains(&admitted),
            "one second of 60fps capture must yield ~30 admitted frames, got {admitted}"
        );
    }

    #[test]
    fn a_long_stall_resynchronises_instead_of_bursting() {
        // 10fps: 100ms per frame, so the resync threshold is 200ms of deficit.
        let (mut p, t0) = pacer(10);
        assert!(frame_arrived(&mut p, t0));

        // The loop stalls for five seconds (a full scratch scan, a page-fault storm, the
        // process being descheduled). The frame that finally arrives is due, so it is
        // admitted — but the schedule must not stay five seconds in the past.
        let after_stall = t0 + Duration::from_secs(5);
        assert!(frame_arrived(&mut p, after_stall), "a late frame is still a frame to encode");

        // A catch-up burst would admit every frame until the five seconds were paid off.
        // Nothing in the next 50ms may be admitted: the pacer resynchronised to
        // `after_stall + interval`, so the next frame is due a whole interval later.
        let burst = (1..=5)
            .filter(|i| frame_arrived(&mut p, after_stall + Duration::from_micros(i * 10_000)))
            .count();
        assert_eq!(
            burst, 0,
            "a stalled pacer must resume at the configured rate, not emit a catch-up burst"
        );
        assert!(
            frame_arrived(&mut p, after_stall + 100 * MS),
            "the schedule resumed one interval after the stall"
        );
    }

    #[test]
    fn a_small_lag_is_absorbed_without_resynchronising() {
        let (mut p, t0) = pacer(10);
        assert!(frame_arrived(&mut p, t0));
        // 50ms late: within the threshold, so this is jitter, and the schedule stays on
        // the original grid rather than being nudged forward by the delay.
        assert!(frame_arrived(&mut p, t0 + 150 * MS));
        assert!(
            frame_arrived(&mut p, t0 + 205 * MS),
            "the grid must still be at t0 + 200ms: a resync here would have pushed it to \
             t0 + 250ms and dropped this frame"
        );
    }

    #[test]
    fn asking_whether_a_frame_is_due_does_not_spend_the_slot() {
        // The asymmetry this whole change rests on: the pump asks *before* it pays for a
        // frame (that is what makes the discarded frames free), and only a frame that
        // actually arrives spends a slot. A query that consumed the slot would make the
        // pacer lose an interval every time it was asked, and it could never reach the
        // configured rate.
        let (mut p, t0) = pacer(30);
        for i in 0..20 {
            let now = t0 + Duration::from_micros(i * 2_000);
            assert!(p.is_due(now), "no frame was taken, so no slot may have been spent");
            assert_eq!(p.time_until_due(now), Duration::ZERO, "a due slot has no wait");
        }
        // The frame that finally arrives is the one that spends a slot — one, not twenty.
        assert!(frame_arrived(&mut p, t0 + 20 * MS));
        assert!(!p.is_due(t0 + 30 * MS), "the interval that frame consumed has not elapsed");
        assert!(p.is_due(t0 + 34 * MS), "the next slot is one interval after the last");
    }

    #[test]
    fn a_window_that_has_closed_is_reported_as_frames_per_second() {
        // 30 frames delivered over one second: the achieved rate is the configured rate.
        let mut m = RateMeter::new(Duration::from_secs(1));
        let t0 = Instant::now();
        fps_is(
            m.record(30, t0 + Duration::from_secs(1)),
            30.0,
            "the window that closed at 1.0s carried 30 frames",
        );        // The next window is measured from its own start, not from the run's.
        fps_is(
            m.record(15, t0 + Duration::from_millis(1_500)),
            30.0,
            "15 frames in the half second since the window before",
        );
    }

    #[test]
    fn the_rate_is_zero_until_a_window_has_closed() {
        // The log line is emitted every ~200ms; a rate computed over the first of those is
        // noise, not a rate, and printing it would make the field actively misleading (a
        // 6-frame window reads 30fps, a 5-frame one 25fps). So the meter reports nothing
        // until a whole window has been measured — a fresh meter's rate is 0.0.
        let mut m = RateMeter::new(Duration::from_secs(1));
        let t0 = Instant::now();
        assert_eq!(m.record(6, t0 + Duration::from_millis(200)), 0.0);
        assert_eq!(m.record(6, t0 + Duration::from_millis(400)), 0.0);
        assert_eq!(m.record(6, t0 + Duration::from_millis(600)), 0.0);
        assert_eq!(m.record(6, t0 + Duration::from_millis(800)), 0.0);
        fps_is(m.record(6, t0 + Duration::from_millis(1_000)), 30.0, "the window closed");
    }

    #[test]
    fn a_window_is_divided_by_the_time_it_really_covered() {
        // The window closes on the first record *after* it is due, so it can be a tick
        // longer than nominal. Dividing by the nominal length instead would inflate the
        // rate — and this is the number that decides whether the machine is keeping up.
        let mut m = RateMeter::new(Duration::from_secs(1));
        let t0 = Instant::now();
        fps_is(
            m.record(30, t0 + Duration::from_millis(1_200)),
            25.0,
            "30 frames over the 1.2s the window really covered",
        );
    }

    #[test]
    fn the_meter_reports_the_frames_that_reached_the_encoder_not_the_ones_admitted() {
        // The measured failure in numbers: the pacer admits 30fps and the encoder's queue
        // drops six of them, so 24 frames per second reach the encoder. The meter must say
        // 24 — the difference between the two is exactly what the warning and the runbook
        // read, and a meter fed from `frames=` (which counts submissions) would say 30.
        let mut m = RateMeter::new(Duration::from_secs(1));
        let t0 = Instant::now();
        let mut fps = 0.0;
        for (i, reached) in [5u64, 5, 5, 5, 4].into_iter().enumerate() {
            let now = t0 + Duration::from_millis((i as u64 + 1) * 200);
            fps = m.record(reached, now);
        }
        fps_is(fps, 24.0, "30 submitted, 6 dropped: 24 frames per second reached ffmpeg");
    }

    /// Compare a measured rate against the expected one, to within 0.1fps.
    ///
    /// Deliberately not an exact comparison: `RateMeter::new` stamps its window from the
    /// clock itself, so a test's own `t0` is microseconds later than the meter's reference,
    /// and the meter divides by the time the window *really* covered. 0.1fps is orders of
    /// magnitude wider than either, and far narrower than any shortfall this meter exists to
    /// report — a machine that cannot encode at the configured rate is tens of frames per
    /// second away, not tenths.
    fn fps_is(actual: f64, expected: f64, what: &str) {
        assert!(
            (actual - expected).abs() < 0.1,
            "{what}: expected about {expected}fps, measured {actual}"
        );
    }
}
