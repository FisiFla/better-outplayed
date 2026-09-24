//! The frame/audio pump: the rate limiter, the accounting, and the post-roll wait.
//!
//! This is the half of the engine that every path shares — the steady-state loop and the
//! clip trigger both feed the encoder through [`pump_once_counted`] and the same
//! [`FramePacer`], so the two cannot drift apart into two different rates on one encoder.
//!
//! It is deliberately independent of [`crate::Recorder`]: the functions here take the
//! capture backend, the audio backend, the encoder and the ring as arguments, which is
//! what makes them drivable from a test with the synthetic stubs (and with a rawvideo pipe
//! declared for a different size, which is the shape one of those tests pins).

use anyhow::{bail, Context, Result};
use localplay_capture::{AudioBackend, CaptureBackend, Frame};
use localplay_encoder::Encoder;
use localplay_replay::buffer::RingBuffer;
use std::time::{Duration, Instant};

/// A media timeline the pump can wait on: something that finds newly written segments and
/// reports how much footage is on disk.
///
/// Introduced for the full-session mode (Phase 5): [`crate::session::SessionRing`] is the
/// ring's cap-free twin, and the post-roll wait below has to work on either without knowing
/// which it was given. The two methods are the whole of what the wait needs — a scan and a
/// span — so the trait is two methods wide and deliberately not the ring's interface.
///
/// `RingBuffer` implements it here (the trait is this crate's, the type is
/// `localplay-replay`'s, which is what makes the impl legal); its `scan` is
/// `scan_once`, eviction included, exactly as before.
pub trait MediaRing {
    /// Find newly written segments and index them.
    fn scan(&mut self) -> Result<()>;
    /// Media time the ring can prove is on disk, in ms, run-relative.
    fn span_ms(&self) -> u64;
}

impl MediaRing for RingBuffer {
    fn scan(&mut self) -> Result<()> {
        self.scan_once()
    }

    fn span_ms(&self) -> u64 {
        self.stats().span_ms
    }
}

/// Longest a single `next_frame` call waits for a frame to become due.
///
/// The capture backends are real-time paced and return `None` when nothing is due
/// within the timeout, so this doubles as the pump's tick: a frame that is already due
/// comes back immediately, otherwise the call sleeps at most this long. It is what
/// keeps the loop off 100% CPU between frames — there is no separate `sleep` in the
/// pump, because sleeping is exactly what starves the encoder.
pub const FRAME_POLL: Duration = Duration::from_millis(5);

/// How often the scratch directory is re-scanned while waiting for a post-roll.
///
/// A scan walks the scratch directory and stats each new segment; nothing in it can
/// change faster than one segment per `buffer.segment_time`, so scanning on every pump
/// tick would spend I/O for no new information. 50ms matches the polling granularity
/// the verification runbook documents for criterion 3 (the clip must be written within
/// `post_seconds + 2s` of the keypress, of which this bounds part of the latency).
pub const POST_ROLL_SCAN_INTERVAL: Duration = Duration::from_millis(50);

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
/// It also carries one unit conversion: the budget is wall clock while `post_ms` is media
/// time, and media time is only as far ahead as the ing has *finished writing*. On the
/// measured 4K box before the timeline fix media ran at 0.81x of the wall clock, which made
/// `post_ms` of media cost ~1.23x `post_ms` of waiting; the two now advance together, and
/// this margin covers that case and the encoder's own lag besides, for any `post_seconds` up
/// to ~20s.
pub const POST_ROLL_MARGIN: Duration = Duration::from_secs(5);

/// How far the pacer may fall behind the wall clock before it resynchronises.
///
/// Two frame intervals. Anything below one interval is ordinary scheduling jitter and
/// must be absorbed by the next frame being admitted a little early; anything above this
/// means the loop genuinely stalled (a slow scan of a full scratch directory, a GC-like
/// pause, a page fault), and catching up frame by frame from there would mean submitting
/// faster than `fps` for as long as the deficit lasts — a burst that re-creates exactly
/// the timeline skew the pacer exists to remove. See [`FramePacer::commit`].
pub const PACER_RESYNC_AFTER_INTERVALS: u32 = 2;

/// How long the achieved frame rate is measured over.
///
/// The status line is emitted every ~200ms (the ring scan interval). A 200ms window holds
/// ~6 frames at the configured 30fps, so the rate it yields quantises to ±5fps and swings
/// around the configured value — useless for the one thing this number is for, which is
/// telling a reader whether the encoder is keeping up. A full second averages ~30 frames:
/// stable enough to read at a glance, and short enough that a shortfall (a slide, a
/// competing GPU load, a resolution switch) is visible within a second of starting.
pub const RATE_WINDOW: Duration = Duration::from_secs(1);

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
/// without the GPU copy that materialising it costs (see [`pump_once_counted`]). On the
/// measured 4K machine that was ~45% of all frames delivered.
///
/// Deliberately has no backlog: it is a rate limiter, not a scheduler. Nothing downstream
/// needs the frames it drops (each one is superseded by the next).
pub struct FramePacer {
    /// The rate this pacer admits — the number the encoder child was told, kept so the two
    /// can be compared from outside (see [`FramePacer::fps`]).
    fps: u32,
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
    ///
    /// **This number must be the same one the encoder child is told.** The pacer admits what
    /// the encoder is expecting to receive: pace to 30 while the child encodes a declared 24
    /// (or the other way round) and the pipeline declares a rate it does not deliver, which
    /// is exactly the defect behind issues #1 and #2. The engine builds both from one
    /// binding — see `Recorder::start_with_measure` — and `EncodeConfig::fps` is that binding.
    pub fn new(fps: u32) -> Self {
        let fps = fps.max(1);
        let interval = Duration::from_nanos(1_000_000_000 / u64::from(fps));
        Self {
            fps,
            next_due: Instant::now(),
            interval,
            resync_after: interval * PACER_RESYNC_AFTER_INTERVALS,
        }
    }

    /// The rate this pacer admits, in frames per second.
    ///
    /// Exists so the rate the *pacer* is actually running at is observable rather than
    /// assumed: the engine publishes it (the status line's denominator and
    /// `RecorderStatus::effective_fps`), and the agreement test between the pacer and the
    /// encoder child reads it back from both sides.
    pub fn fps(&self) -> u32 {
        self.fps
    }

    /// Whether a frame is due at `now` — i.e. whether the caller should take one off the
    /// capture backend at all.
    ///
    /// `false` means *do not take a frame*: nothing may be submitted yet, and the frames
    /// the backend is holding are surplus. Asking this **before** the frame exists is what
    /// lets the caller skip the readback for the frames it will not keep; asking it (as
    /// this used to be written, as `admit`) after the frame had already been materialised
    /// meant paying for the copy and then throwing it away.
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

impl PumpCounts {
    /// The two counters added together, for a caller accumulating several pumps.
    pub fn plus(self, other: PumpCounts) -> PumpCounts {
        PumpCounts {
            submitted: self.submitted + other.submitted,
            skipped: self.skipped + other.skipped,
        }
    }
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
/// "keep the encoder fed" lives in exactly one place. The counts exist so the engine can
/// keep the status line's `frames=` counter while that rule stays centralised — and so the
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
/// 3. Submit it, once [`guard_frame_size`] has checked it against the size the encoder's
///    rawvideo pipe was declared with.
///
/// The not-due branch waits (bounded by [`FRAME_POLL`], never past the next slot). It has
/// to: `discard_pending` never blocks, unlike `next_frame`, so a loop with no wait here
/// would spin at 100% CPU for the rest of every interval — the exact cost this change
/// exists to remove. Sleeping until the slot cannot starve the encoder (a frame the pacer
/// is not due for is a frame it will not keep), and the cap keeps the caller's other work
/// — the hotkey poll, the ring scan, the periodic status line — at the granularity it has
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
    pump_once_counted_with_mic(pacer, capture, audio, None, encoder)
}

/// [`pump_once_counted`], with the microphone input drained too.
///
/// The microphone is a **second input with its own writer thread inside the encoder**, fed
/// from this same loop iteration: the game audio goes to `submit_audio` and the microphone
/// to `submit_mic_audio`, both of which hand a block to a queue that a thread of their own
/// drains into its own loopback socket. One pump thread submitting to two queues is the
/// shape the encoder's own docs require — a synchronous write of both inputs from one
/// thread is the deadlock this pipeline already fixed once — and it is why the microphone
/// adds no thread of its own here.
///
/// `mic` is `None` whenever this recording has no microphone track (`[mic] enabled = false`,
/// the default): the function is then exactly the pre-Phase-5 pump, one `submit_audio` loop
/// and nothing else.
pub fn pump_once_counted_with_mic(
    pacer: &mut FramePacer,
    capture: &mut dyn CaptureBackend,
    audio: &mut dyn AudioBackend,
    mic: Option<&mut (dyn AudioBackend + '_)>,
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
    // The microphone's blocks are drained the same way, for the same reason: its timeline
    // is the exact sample count, and a block left in the backend is a hole in the voice
    // track rather than a frame that is merely late.
    if let Some(mic) = mic {
        while let Some(block) = mic.next_buffer(Duration::ZERO)? {
            encoder.submit_mic_audio(block)?;
        }
    }
    Ok(counts)
}

/// Refuse a frame whose geometry disagrees with the rawvideo pipe's declared size.
///
/// The pipe is a flat byte stream that ffmpeg slices into frames of the size it was
/// spawned with (`EncodeConfig::source_size`, i.e. `Encoder::source_size`); it carries
/// no framing of its own, so a frame of a different size is not rejected by anything.
/// It is *mis-read*: every boundary after the first lands mid-frame, and the picture
/// comes apart in diagonal bands while the segment files still look healthy and the
/// logs stay clean. That is a silent-corruption class of bug, and this project found
/// it the hard way: on Windows 11 a 4K desktop at 150% scaling had the rawvideo pipe
/// declared 2560x1440 (logical pixels, from a DPI-virtualised `GetSystemMetrics`) while
/// the capture item was 3840x2160 physical pixels — the size every frame carries.
///
/// The two values are derived from one another in the engine's `build_encode_config` —
/// `native_size` builds the `EncodeConfig` and this compares the frames against it — so a
/// mismatch means that link is broken (a resolution change mid-capture, or a backend
/// reporting a size it does not deliver). Either way the safe answer is to stop, loudly,
/// rather than hand ffmpeg bytes it will misread.
pub fn guard_frame_size(frame: &Frame, configured: (u32, u32)) -> Result<()> {
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
/// This is the trigger's post-roll wait (spec §6.2 step 2), and it is deliberately
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
/// two clocks in one call: `need_ms` and the span are media time, `budget` is wall clock.
/// They now advance together (the timeline fix), but media time still only counts segments
/// ffmpeg has finished, so the caller adds a margin rather than passing a bare `post_ms`.
///
/// Returns the counts of what it pumped while waiting, so the caller's `frames=` and
/// `skipped=` account for every frame that reached the encoder — including the ones the
/// wait itself submitted. Returns `Ok` once the span covers `need_ms`; a span already at
/// or beyond `need_ms` on entry returns immediately. Errors if the budget elapses first,
/// or if the capture/encode path fails.
pub fn pump_until_span(
    pacer: &mut FramePacer,
    ring: &mut RingBuffer,
    capture: &mut dyn CaptureBackend,
    audio: &mut dyn AudioBackend,
    encoder: &mut dyn Encoder,
    need_ms: u64,
    budget: Duration,
) -> Result<PumpCounts> {
    pump_until_span_on(pacer, ring, capture, audio, None, encoder, need_ms, budget)
}

/// [`pump_until_span`] over any [`MediaRing`], with an optional microphone input.
///
/// Two things vary between the callers and nothing else does: which ledger the span comes
/// from (the replay ring, or a full session's own segments — see
/// [`crate::session::SessionRing`]) and whether a microphone is drained alongside the game
/// audio. Both are parameters rather than a second wait loop, because this loop is the one
/// place that must keep the encoder fed while it waits, and a second copy of it is a second
/// chance to get that wrong.
// Seven parameters plus the ring: the wait loop's handles are what it needs to keep feeding
// one encoder while it waits, and bundling them into a struct would only move the list. The
// pre-Phase-5 loop took seven for the same reason; the microphone is the eighth.
#[allow(clippy::too_many_arguments)]
pub fn pump_until_span_on(
    pacer: &mut FramePacer,
    ring: &mut (dyn MediaRing + '_),
    capture: &mut dyn CaptureBackend,
    audio: &mut dyn AudioBackend,
    mic: Option<&mut (dyn AudioBackend + '_)>,
    encoder: &mut dyn Encoder,
    need_ms: u64,
    budget: Duration,
) -> Result<PumpCounts> {
    let started = Instant::now();
    let deadline = started + budget;
    let mut counts = PumpCounts::default();
    // Scan on the first pass, then on the interval: the span can only move when
    // ffmpeg finalises a segment, which is a per-segment event, not a per-frame one.
    let mut next_scan = started;
    // The microphone is borrowed for the whole wait: re-borrowing it inside the loop would
    // mean a second `Option<&mut dyn AudioBackend>` per iteration, which the borrow checker
    // rightly refuses.
    let mut mic = mic;

    loop {
        counts = counts.plus(pump_once_counted_with_mic(
            pacer,
            capture,
            audio,
            mic.as_deref_mut(),
            encoder,
        )?);

        if Instant::now() >= next_scan {
            ring.scan().context("scanning for the post-roll")?;
            next_scan = Instant::now() + POST_ROLL_SCAN_INTERVAL;
            let span = ring.span_ms();
            if span >= need_ms {
                tracing::debug!(
                    "post-roll on disk: span={span}ms covers {need_ms}ms after {}ms \
                     ({} frames submitted while waiting)",
                    started.elapsed().as_millis(),
                    counts.submitted
                );
                return Ok(counts);
            }
        }

        if Instant::now() >= deadline {
            bail!(
                "timed out after {}ms waiting for post-roll (span={}ms need={}ms): the \
                 encoder produced no segment covering the trigger",
                started.elapsed().as_millis(),
                ring.span_ms(),
                need_ms
            );
        }
    }
}

/// The rate frames are actually reaching the encoder at, averaged over `window`.
///
/// This is the rate the *encoder child is given frames at* — not the rate the pacer admitted,
/// and not the rate the source offered. The three differ exactly when the machine cannot
/// encode the declared rate (the case this is here to expose), and the gap between the first
/// two is what the drop warning is about: frames that were captured, copied and never
/// encoded, so the picture holds instead. It does **not** decide whether media time tracks
/// real time — that is the frames' arrival timestamps (see `localplay_encoder::ffmpeg`), and
/// it holds at any rate including this one, which is why the warning it feeds no longer
/// claims otherwise.
///
/// `record` is called on every pump with the frames that survived since the previous call,
/// so the measurement covers the whole run rather than whichever instant a log line
/// happened to sample. The reported value is the last *completed* window, which is why a
/// fresh meter reports 0.0: the first second of a run has no measured window yet. That is
/// deliberate — a rate computed over the first 200ms of a run is not a rate, it is noise.
#[derive(Debug)]
pub struct RateMeter {
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
    pub fn new(window: Duration) -> Self {
        Self { window, frames: 0, since: Instant::now(), fps: 0.0 }
    }

    /// Add `frames` to the open window and return the achieved rate.
    ///
    /// `now` is a parameter rather than a fresh `Instant::now()` so the meter's arithmetic
    /// is testable without sleeping: nothing here is timing-dependent, only
    /// time-*stamped*.
    pub fn record(&mut self, frames: u64, now: Instant) -> f64 {
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
        );
        // The next window is measured from its own start, not from the run's.
        fps_is(
            m.record(15, t0 + Duration::from_millis(1_500)),
            30.0,
            "15 frames in the half second since the window before",
        );
    }

    #[test]
    fn the_rate_is_zero_until_a_window_has_closed() {
        // The status line is emitted every ~200ms; a rate computed over the first of those
        // is noise, not a rate, and printing it would make the field actively misleading (a
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
