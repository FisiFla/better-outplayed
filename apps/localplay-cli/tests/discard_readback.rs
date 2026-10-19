//! Regression test for the pacer's discard path: a frame the pacer will not keep must never be
//! read back.
//!
//! The defect, measured on the Windows test machine (3840x2160 at 144Hz, `fps = 30`): the
//! capture backend delivered 53-75 frames per second, the pacer admitted 30, and the discard
//! happened after the expensive work. `pump_once_counted` called `capture.next_frame(..)` first
//! (for WGC the whole staging-texture GPU readback and a 33.2MB CPU copy) and only then asked
//! the pacer, so ~45% of readbacks were paid for and thrown away, costing 50.8% of a CPU core.
//!
//! The fix asks [`FramePacer::is_due`] before taking a frame and closes the surplus with
//! [`CaptureBackend::discard_pending`], which copies nothing. These tests assert the split
//! between materialised and discarded, which is the whole optimisation.
//!
//! * `a_frame_the_pacer_is_not_due_for_is_never_materialised` is fully deterministic: one pump
//!   call, with the pacer's slot spent by hand.
//! * `a_source_faster_than_the_pacer_is_discarded_without_being_read_back` is a one-second
//!   real-time run: ~30 frames materialised (the configured rate) while a source four times
//!   faster offers ~120. The load-bearing assertions are the counter identities
//!   (`submitted == read back`, `skipped == discarded`) and the ordering
//!   (`discarded > read_back`); the bands around 30 and 50 are generous so scheduling jitter on
//!   a loaded CI machine does not fail it.
//!
//! Gating: deliberately not `#![cfg(feature = "test-encoders")]`, which is enabled on the
//! encoder package, so a `cfg` gate here would run an empty test binary. It needs
//! `EncodeConfig::for_tests_software`, which exists because `localplay-encoder` is a
//! dev-dependency here with that feature on (`apps/localplay-cli/Cargo.toml`).
//!
//! Needs ffmpeg on `PATH` with a working libx264.

// The test bodies use `unwrap()` throughout; production code does not.
#![allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks)]
use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_encoder::{EncodeConfig, Encoder, FfmpegEncoder, VideoCodec};
use localplay_media::FfmpegBinaries;
use localplay_recorder::{pump_once_counted, FramePacer, PumpCounts};
use std::time::{Duration, Instant};

/// What the synthetic source offers: four times the pacer's target. A source rate-matched to
/// the target has no surplus, which is why the defect went unnoticed until a high-refresh
/// display was measured.
const SOURCE_FPS: u32 = 120;
/// What the pacer is configured for, i.e. `encode.fps`.
const TARGET_FPS: u32 = 30;
/// Small frames: the test is about how many frames are touched, not pixels. The stub is a
/// synthetic gradient.
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
        let mut capture = StubCapture::new(StubConfig {
            width: WIDTH,
            height: HEIGHT,
            fps: SOURCE_FPS,
        });
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
        pump_once_counted(
            &mut self.pacer,
            &mut self.capture,
            Some(&mut self.audio),
            &mut self.encoder,
        )
        .expect("a pump iteration must not fail")
    }
}

/// The deterministic half: one pump call, aimed at a slot the pacer has already spent.
///
/// Spending the first slot by hand leaves the pacer not due for a whole interval (33.3ms at
/// 30fps), far more slack than needed, without sleeping. The source meanwhile holds a due
/// frame, so the pump must deal with one the pacer will not keep, and dealing with it must cost
/// no readback.
#[test]
fn a_frame_the_pacer_is_not_due_for_is_never_materialised() {
    let mut fx = Fixture::new();

    // Spend the first slot, exactly as the pump does once a frame exists.
    let t0 = Instant::now();
    assert!(
        fx.pacer.is_due(t0),
        "sanity: the first slot is due immediately"
    );
    fx.pacer.commit(t0);
    assert!(
        !fx.pacer.is_due(t0),
        "and it is spent: the next one is an interval away"
    );

    // 120fps source: its first frame is due the moment capture starts, before this test began. It
    // is waiting, and the pump must not take it.
    let counts = fx.pump();
    eprintln!(
        "one pump call with the slot spent: submitted={} skipped={} read_back={} \
         discarded={}",
        counts.submitted,
        counts.skipped,
        fx.capture.frames_read_back(),
        fx.capture.frames_discarded()
    );

    assert_eq!(
        counts.submitted, 0,
        "the pacer is not due, so nothing may be submitted"
    );
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

    // --- Deterministic: the pump's counters and the backend's are one account. Every offered frame
    // was either taken (and submitted) or consumed without being rendered. This fails if the pump
    // stops counting a discard or the stub renders a frame it was asked to discard, independent of
    // timing.
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

    // --- The load-bearing ordering: with a source four times faster than the target, the pacer
    // throws away far more than it keeps. The old order read every offered frame back and dropped
    // ~3/4 of them.
    assert!(
        discarded > read_back,
        "a 4x-faster source must lose more frames than it contributes: {discarded} discarded \
         vs {read_back} materialised"
    );
    // No absolute floor on `discarded`: the two counts are governed by different things.
    // `read_back` is the pacer's rate (~TARGET_FPS/s), so its band is stable, while `discarded` is
    // the source's offered rate minus that, and on a loaded runner the stub source is itself
    // starved (CI saw only 34 discarded). An absolute floor would measure the machine, not the
    // pump. A pump that stopped taking the discard path would make `discarded` 0 and fail
    // `discarded > read_back` above; the counter accounting pins the behaviour exactly.

    // --- The configured rate, not the source's. Generous bands: exact counts depend on where the
    // run starts relative to the frame grid and on scheduling under load; what is asserted is that
    // materialised frames are the pacer's rate (~30/s), not the source's (~120/s).
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
