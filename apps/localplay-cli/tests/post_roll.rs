//! Regression test for the hotkey's post-roll wait (spec §6.2 step 2).
//!
//! The bug this file exists to prevent: the trigger path waited for the post-roll with
//! a loop that only slept and re-scanned the scratch directory, never calling
//! `capture.next_frame` / `encoder.submit_video` / `submit_audio`. ffmpeg therefore
//! received no data, produced no new segment files, the buffer's `span_ms` could not
//! advance, and **every** `Ctrl+F8` press ended in
//! `timed out waiting for post-roll` and a non-zero exit. Nothing caught it because the
//! trigger is reached through a global hotkey that cannot fire on the macOS host this
//! is developed on, so the code had never run once.
//!
//! The hotkey itself is still untested (it needs Windows). What *is* tested here is the
//! wait it drives, in isolation: `localplay_cli::pump_until_span` over stub capture
//! sources and a real ffmpeg encoder.
//!
//! Gating: this file is deliberately **not** `#![cfg(feature = "test-encoders")]`.
//! The suite runs as `cargo test --workspace --features
//! localplay-encoder/test-encoders`, which enables that feature on the *encoder*
//! package and not on this one — a `cfg` gate here would be off and Cargo would run an
//! empty test binary, silently dropping the regression test from the suite. (Measured:
//! with the gate in place, `cargo test -p localplay-cli --features
//! localplay-encoder/test-encoders --test post_roll` reports `running 0 tests`.)
//! A test that can compile itself away is the same class of failure this file exists
//! to catch, so the dependency is guaranteed instead: `localplay-encoder` is a
//! dev-dependency of this package with `test-encoders` enabled (Cargo.toml), which
//! makes `EncodeConfig::for_tests_software` exist in every test build of this package —
//! and in no shipping one.
//!
//! The test needs ffmpeg on `PATH` with a working libx264, like the rest of the suite.

use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_cli::{pump_until_span, FramePacer};
use localplay_encoder::{EncodeConfig, Encoder, FfmpegEncoder, VideoCodec};
use localplay_media::{FfmpegBinaries, MediaInfo};
use localplay_replay::buffer::{BufferConfig, RingBuffer};
use std::time::{Duration, Instant};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const FPS: u32 = 10;
const SEGMENT_MS: u64 = 1_000;
const PRE_MS: u64 = 2_000;
const POST_MS: u64 = 2_000;
/// Timeline handed to the encoder up front, so the buffer has content to extend.
///
/// This is now *wall-clock* time, not a frame count: the encoder's media timeline is the
/// wall clock (see the module comment in `localplay_encoder::ffmpeg`), so the fixture feeds
/// its stubs in real time — through the same `pump_once` the CLI uses — for this long.
/// Feeding the same frames in a burst instead would produce almost no footage, which is the
/// behaviour that was fixed, not a property to test around.
const PRE_FED: Duration = Duration::from_secs(3);

/// A live pipeline over stub sources: capture → encoder → ring buffer.
///
/// Owns its temp directories, so they outlive the pipeline that writes into them.
struct Fixture {
    _scratch: tempfile::TempDir,
    _clips: tempfile::TempDir,
    bin: FfmpegBinaries,
    ring: RingBuffer,
    capture: StubCapture,
    audio: StubAudio,
    encoder: FfmpegEncoder,
    /// The same rate limit the CLI's own loop applies, at the stub's frame rate, so the
    /// wait under test is driven exactly as the CLI drives it.
    pacer: FramePacer,
}

impl Fixture {
    fn new() -> Self {
        let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
        let scratch = tempfile::tempdir().unwrap();
        let clips = tempfile::tempdir().unwrap();

        let encode = EncodeConfig::for_tests_software(
            VideoCodec::H264,
            WIDTH,
            HEIGHT,
            FPS,
            scratch.path().to_path_buf(),
            SEGMENT_MS,
        );
        let cfg = BufferConfig {
            pre_ms: PRE_MS,
            post_ms: POST_MS,
            scratch_cap_bytes: 1 << 30,
            segment_ms: SEGMENT_MS,
            clips_dir: clips.path().to_path_buf(),
        };

        let mut capture = StubCapture::new(StubConfig { width: WIDTH, height: HEIGHT, fps: FPS });
        let mut audio = StubAudio::new(AudioFormat::default());
        let mut encoder = FfmpegEncoder::spawn(&bin, &encode).expect("spawn encoder");
        let mut ring = RingBuffer::start(
            &bin,
            cfg,
            scratch.path().to_path_buf(),
            "libx264".to_string(),
        )
        .expect("start ring buffer");

        // Feed the pipeline in real time, through the same `pump_once` the CLI's loop
        // and the post-roll wait use. The media timeline comes from the wall clock, so
        // this is what makes the stubs produce footage at all: an unpaced burst of the
        // same frames lands within a few milliseconds of capture time and encodes as a
        // few milliseconds of media — legitimately, but uselessly for a fixture that
        // needs seconds of buffer.
        capture.start().unwrap();
        audio.start().unwrap();
        let mut pacer = FramePacer::new(FPS);
        let feed_until = Instant::now() + PRE_FED;
        while Instant::now() < feed_until {
            localplay_cli::pump_once(&mut pacer, &mut capture, &mut audio, &mut encoder)
                .expect("feeding the fixture's stubs");
        }

        // Submits hand the bytes to the encoder's writer threads, so segment files
        // appear as ffmpeg consumes them, not as they are submitted — and a segment is
        // only trusted once a strictly later one exists. Wait for the ring to index
        // something rather than assuming the fed timeline is already on disk.
        let deadline = Instant::now() + Duration::from_secs(15);
        while ring.stats().span_ms == 0 {
            assert!(
                Instant::now() < deadline,
                "fixture never completed a segment ({:?} of timeline fed)",
                PRE_FED
            );
            std::thread::sleep(Duration::from_millis(20));
            ring.scan_once().expect("scan scratch dir");
        }

        Self { _scratch: scratch, _clips: clips, bin, ring, capture, audio, encoder, pacer }
    }

    /// Exactly the call the hotkey branch makes.
    fn pump(&mut self, need_ms: u64, budget: Duration) -> anyhow::Result<()> {
        pump_until_span(
            &mut self.pacer,
            &mut self.ring,
            &mut self.capture,
            &mut self.audio,
            &mut self.encoder,
            need_ms,
            budget,
        )
    }
}

#[test]
fn the_post_roll_wait_keeps_feeding_ffmpeg_so_the_span_advances() {
    let mut fx = Fixture::new();

    // One iteration of the steady-state pump the CLI loop uses and `pump_until_span`
    // shares. The stub clock has caught up with real time by now, so whether a frame is
    // due does not matter: what matters is that a pump iteration cannot fail.
    localplay_cli::pump_once(&mut fx.pacer, &mut fx.capture, &mut fx.audio, &mut fx.encoder)
        .expect("a pump iteration must not fail");

    let before = fx.ring.stats();
    assert!(
        before.span_ms > 0,
        "fixture must leave timeline in the buffer, got {}ms",
        before.span_ms
    );

    let need_ms = before.span_ms + 2_000;
    let started = Instant::now();
    let result = fx.pump(need_ms, Duration::from_secs(60));
    let waited = started.elapsed();
    eprintln!(
        "post-roll wait: span {}ms -> need {}ms; returned {result:?} after {waited:?}",
        before.span_ms, need_ms
    );

    result.expect("the wait must reach the post-roll, not time out");
    let after = fx.ring.stats();
    eprintln!(
        "after the wait: span={}ms segments={} (was span={}ms segments={})",
        after.span_ms, after.segments, before.span_ms, before.segments
    );

    // The load-bearing assertion: the span only moves when ffmpeg finalises a segment,
    // and ffmpeg only finalises a segment while frames keep arriving. A wait that does
    // not pump therefore cannot satisfy this, however long its budget.
    assert!(
        after.span_ms >= need_ms,
        "span must advance past {need_ms}ms while waiting: it was {}ms before and {}ms after",
        before.span_ms,
        after.span_ms
    );
    assert!(
        after.segments > before.segments,
        "the wait must add completed segments: {} before, {} after",
        before.segments,
        after.segments
    );
    // Sanity check that the wait really ran rather than skipping ahead. This used to read
    // `waited >= 2s` — "the 2s of post-roll cannot be captured instantly" — which held
    // while the media timeline was a frame count fed only by this pump. It is not a
    // property of the system any more, and the change is not the wait's: the media timeline
    // is now the wall clock, so media time and wall time are only equal *at the capture
    // source*. ffmpeg holds frames in its own lookahead and in the pipe between the two
    // (measured here: the wait returned after 1.10s having covered 2000ms of media), so a
    // wait may legitimately finish sooner than `post_ms` of wall clock once that buffer is
    // drained. What must hold — "the media covering need_ms is on disk" — is asserted
    // above, and it is the assertion that fails when the wait does not pump. The floor
    // below only says the wait was not instantaneous.
    assert!(
        waited >= Duration::from_millis(100),
        "the wait must take real time at all; it returned after {waited:?}"
    );

    // What the wait is *for*. The CLI's next step is `ring.trigger(trigger_ms, ..)`,
    // which refuses to splice until the ledger covers `trigger_ms + post_ms`
    // (`WindowError::PostRollUnavailable` — what a user would see as "no clip was ever
    // written"). So the wait is only correct if the trigger it precedes succeeds and
    // produces a clip carrying both streams. Here `trigger_ms` is the instant the wait
    // has just made available: `need_ms` is exactly `trigger_ms + post_ms`.
    let trigger_ms = need_ms - POST_MS;
    let clip = fx
        .ring
        .trigger(trigger_ms, "post-roll-clip")
        .expect("the post-roll must be on disk once the wait returns");
    let info = MediaInfo::probe(&fx.bin, &clip.path).expect("probe the clip");
    eprintln!(
        "clip after the wait: {} ({}ms, {} bytes, encoder={})",
        clip.path.display(),
        info.duration_ms,
        info.size_bytes,
        clip.encoder
    );
    assert!(clip.path.is_file(), "the clip must exist on disk");
    assert!(info.video.is_some(), "clip must have video");
    assert!(info.audio.is_some(), "clip must have audio");
    assert_eq!(clip.encoder, "libx264");

    fx.encoder.finish().expect("flush encoder");
}

#[test]
fn an_unreachable_need_ms_gives_up_on_its_budget_instead_of_hanging() {
    let mut fx = Fixture::new();

    let budget = Duration::from_secs(1);
    let started = Instant::now();
    let err = fx
        .pump(10_000_000, budget)
        .expect_err("an unreachable need_ms must fail");
    let waited = started.elapsed();
    eprintln!("gave up after {waited:?}: {err}");

    assert!(
        waited >= Duration::from_millis(900),
        "the wait must actually run for its budget; gave up after {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(30),
        "the budget must end the wait, not hang the process; gave up after {waited:?}"
    );
    assert!(
        err.to_string().contains("timed out"),
        "the error must name the failure: {err}"
    );

    fx.encoder.finish().expect("flush encoder");
}
