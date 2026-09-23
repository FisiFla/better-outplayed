//! Regression test for the pacer's *discard* path: a frame the pacer will not keep must
//! never be read back.
//!
//! The defect this file pins, measured on the box (RTX 3090, 3840x2160 at 144Hz, configured
//! `fps = 30`): the capture backend delivered 53-75 frames per second, the pacer admitted
//! 30, and **the discard happened after the expensive work**. `pump_once_counted` called
//! `capture.next_frame(..)` first — for WGC that is the whole staging-texture GPU readback
//! (`CopyResource` + `Map` + a row-by-row CPU copy of 33.2MB) — and only then asked the
//! pacer whether to keep the frame. So ~45% of every readback and every copy was paid for
//! and thrown away, and the app burned 50.8% of a CPU core doing it.
//!
//! The fix inverts that order: ask [`FramePacer::is_due`] *before* taking a frame, and close
//! the surplus with [`CaptureBackend::discard_pending`], which copies nothing. What this
//! test asserts is exactly the split between the two — materialised versus discarded —
//! because that split is the entire optimisation. With the old order of operations the
//! "discarded" count is 0 and the "materialised" count is the *source's* rate (120/s here),
//! so every assertion below fails.
//!
//! Two tests, deliberately of different kinds:
//!
//! * `a_frame_the_pacer_is_not_due_for_is_never_materialised` is **fully deterministic**:
//!   one pump call, no assumption about rates, only the ~33ms of slack the pacer is given
//!   by hand (test overhead is microseconds).
//! * `a_source_faster_than_the_pacer_is_discarded_without_being_read_back` is a one-second
//!   real-time run that pins the *rate*: ~30 frames materialised (the configured rate) while
//!   a source four times faster offers ~120. The load-bearing assertions are the identity
//!   between the pump's counters and the backend's (`submitted == read back`,
//!   `skipped == discarded`) and the ordering (`discarded > read_back`); the bands around 30
//!   and 50 are generous on purpose, because a test that encodes scheduling jitter as a
//!   tight bound would fail on a loaded CI machine for no reason.
//!
//! Gating: like `post_roll.rs` and `frame_size_mismatch.rs`, this file is deliberately
//! **not** `#![cfg(feature = "test-encoders")]` — the suite enables that feature on the
//! *encoder* package (`--features localplay-encoder/test-encoders`), so a `cfg` gate here
//! would be off and Cargo would run an empty test binary, silently dropping the test. It
//! needs `EncodeConfig::for_tests_software`, which exists because `localplay-encoder` is a
//! dev-dependency of this package with that feature enabled (apps/localplay-cli/Cargo.toml).
//!
//! Needs ffmpeg on `PATH` with a working libx264, like the rest of the suite.

use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_recorder::{pump_once_counted, FramePacer, PumpCounts};
use localplay_encoder::{EncodeConfig, Encoder, FfmpegEncoder, VideoCodec};
use localplay_media::FfmpegBinaries;
use std::time::{Duration, Instant};

/// What the synthetic source offers. Deliberately four times the pacer's target: the box
/// delivered 53-75fps against a configured 30, and a source rate-matched to the target has
/// no surplus at all — which is exactly why the defect went unnoticed until a high-refresh
/// display was measured.
const SOURCE_FPS: u32 = 120;
/// What the pacer is configured for, i.e. `encode.fps`.
const TARGET_FPS: u32 = 30;
/// Small frames: this test is about how many frames are *touched*, not about pixels. Nothing
/// here is a capture of the screen — the stub is a synthetic gradient.
const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const SEGMENT_MS: u64 = 1_000;
/// The real-time run length. The counters below are rates × this.
const RUN: Duration = Duration::from_secs(1);

/// A stub source feeding a real ffmpeg encoder, with the same rate limit the CLI applies.
struct Fixture {
    /// Owns the encoder's scratch directory, so it outlives the encoder.
    _scratch: tempfile::TempDir,
    capture: StubCapture,
    audio: StubAudio,
    encoder: FfmpegEncoder,
    pacer: FramePacer,
}

impl Fixture {
    fn new() -> Self {
        let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
        let scratch = tempfile::tempdir().expect("a scratch temp dir");
        let encode = EncodeConfig::for_tests_software(
            VideoCodec::H264,
            WIDTH,
            HEIGHT,
            TARGET_FPS,
            scratch.path().to_path_buf(),
            SEGMENT_MS,
        );
        let mut capture =
            StubCapture::new(StubConfig { width: WIDTH, height: HEIGHT, fps: SOURCE_FPS });
        let mut audio = StubAudio::new(AudioFormat::default());
        let encoder = FfmpegEncoder::spawn(&bin, &encode).expect("spawn encoder");
        capture.start().expect("start the capture stub");
        audio.start().expect("start the audio stub");
        Self {
            _scratch: scratch,
            capture,
            audio,
            encoder,
            pacer: FramePacer::new(TARGET_FPS),
        }
    }

    fn pump(&mut self) -> PumpCounts {
        pump_once_counted(&mut self.pacer, &mut self.capture, &mut self.audio, &mut self.encoder)
            .expect("a pump iteration must not fail")
    }
}

/// The deterministic half: one pump call, aimed at a slot the pacer has already spent.
///
/// The pacer's first slot is spent by hand, which leaves it not due for a whole interval
/// (33.3ms at 30fps) — orders of magnitude more slack than this test needs, and it never
/// sleeps to get it. The source, meanwhile, is sitting on a frame that is due right now. So
/// the frame the pump must deal with is unambiguously one the pacer will not keep, and the
/// property under test is that dealing with it costs no readback at all.
#[test]
fn a_frame_the_pacer_is_not_due_for_is_never_materialised() {
    let mut fx = Fixture::new();

    // Spend the first slot, exactly as the pump does once a frame exists.
    let t0 = Instant::now();
    assert!(fx.pacer.is_due(t0), "sanity: the first slot is due immediately");
    fx.pacer.commit(t0);
    assert!(!fx.pacer.is_due(t0), "and it is spent: the next one is an interval away");

    // 120fps source: its first frame is due the moment capture starts, which was before
    // this test began. It is waiting, and the pump is not allowed to take it.
    let counts = fx.pump();
    eprintln!(
        "one pump call with the slot spent: submitted={} skipped={} read_back={} \
         discarded={}",
        counts.submitted,
        counts.skipped,
        fx.capture.frames_read_back(),
        fx.capture.frames_discarded()
    );

    assert_eq!(counts.submitted, 0, "the pacer is not due, so nothing may be submitted");
    assert_eq!(
        fx.capture.frames_read_back(),
        0,
        "a frame the pacer will not keep must not be materialised: this readback is the \
         whole cost the fix removes"
    );
    assert_eq!(
        counts.skipped, 1,
        "the frame the source had waiting must be discarded (and counted), not left in place"
    );
    assert_eq!(fx.capture.frames_discarded(), 1);

    fx.encoder.finish().expect("flush encoder");
}

/// The rate half: one second of a source four times faster than the configured rate.
#[test]
fn a_source_faster_than_the_pacer_is_discarded_without_being_read_back() {
    let mut fx = Fixture::new();

    let mut counts = PumpCounts::default();
    let started = Instant::now();
    while started.elapsed() < RUN {
        let pumped = fx.pump();
        counts.submitted += pumped.submitted;
        counts.skipped += pumped.skipped;
    }
    let elapsed = started.elapsed();
    fx.encoder.finish().expect("flush the encoder");

    let read_back = fx.capture.frames_read_back();
    let discarded = fx.capture.frames_discarded();
    eprintln!(
        "source {SOURCE_FPS}fps, pacer {TARGET_FPS}fps, {elapsed:?}: materialised (read back) \
         = {read_back}, discarded without readback = {discarded}, offered = {}; the pump \
         submitted {read_back} and skipped {discarded}",
        read_back + discarded
    );

    // --- Deterministic: the pump's counters and the backend's are one account. Every frame
    // the source offered was either taken (and submitted) or consumed without being
    // rendered. If a future edit made the pump stop counting a discard, or made the stub
    // render a frame it was asked to discard, this fails without depending on any timing.
    assert_eq!(
        read_back, counts.submitted,
        "the frames the source materialised and the frames the pump submitted must be the \
         same frames: {read_back} materialised, {} submitted",
        counts.submitted
    );
    assert_eq!(
        discarded, counts.skipped,
        "the frames the source discarded and the frames the pump skipped must be the same \
         frames: {discarded} discarded, {} skipped",
        counts.skipped
    );

    // --- The load-bearing ordering, and the one the old code fails outright: with a source
    // four times faster than the target, the pacer throws away far more than it keeps. The
    // old code read *every* offered frame back and then dropped ~3/4 of them, which is the
    // 45% of wasted readback and the 50.8% of a CPU core that motivated the fix.
    assert!(
        discarded > read_back,
        "a 4x-faster source must lose more frames than it contributes: {discarded} discarded \
         vs {read_back} materialised"
    );
    assert!(
        discarded > 50,
        "over a second of a {SOURCE_FPS}fps source against a {TARGET_FPS}fps pacer, the \
         surplus is ~90 frames; only {discarded} were discarded, which means the pump is not \
         taking the discard path"
    );

    // --- The configured rate, not the source's. Generous bands: the exact counts depend on
    // where the run starts relative to the frame grid and on how the loop is scheduled under
    // load, and neither changes what is being asserted — that the materialised frames are
    // the *pacer's* rate (~30/s), not the source's (~120/s).
    assert!(
        (20..=40).contains(&read_back),
        "{read_back} frames were materialised over {elapsed:?} for a {TARGET_FPS}fps target: \
         expected ~{TARGET_FPS}"
    );
    assert!(
        read_back < SOURCE_FPS as u64 / 2,
        "the pump materialised {read_back} frames from a {SOURCE_FPS}fps source: the pacer is \
         not limiting what is read back"
    );
}
