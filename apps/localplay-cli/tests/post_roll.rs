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
//! wait it drives, in isolation: `localplay_recorder::pump_until_span_on` over stub capture
//! sources, a real ffmpeg encoder and the **in-memory** ring the replay buffer actually ships
//! with. That last part is deliberate rather than incidental: the file-backed ring this file
//! used to drive has been removed, and a wait whose span comes from RAM behaves differently in
//! one respect that matters here — a fragment carries its own length, so the span is exact
//! instead of lagging a segment behind. The assertions below were written against both.
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
use localplay_recorder::memory_ring::{MemoryRing, RingSetup};
use localplay_recorder::{pump_once, pump_until_span_on, FramePacer, PumpCounts};
use localplay_encoder::{EncodeConfig, EncodeOutput, Encoder, FfmpegEncoder, VideoCodec};
use localplay_media::{FfmpegBinaries, MediaInfo};
use localplay_replay::MemoryStats;
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

/// A live pipeline over stub sources: capture → encoder → **in-memory** ring.
///
/// Owns its temp directory, so it outlives the clip the pipeline writes into it. There is no
/// scratch directory to own: the buffer keeps its footage in RAM, which is what it is for.
struct Fixture {
    _clips: tempfile::TempDir,
    /// Never written to. `EncodeConfig::for_tests_software` asks for a segment directory because
    /// every other test uses one; this pipeline runs in `FragmentedStream` mode, where ffmpeg
    /// writes to its stdout and no file per second is produced anywhere.
    _segment_dir: tempfile::TempDir,
    bin: FfmpegBinaries,
    ring: MemoryRing,
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
        let segment_dir = tempfile::tempdir().unwrap();
        let clips = tempfile::tempdir().unwrap();

        let mut encode = EncodeConfig::for_tests_software(
            VideoCodec::H264,
            WIDTH,
            HEIGHT,
            FPS,
            segment_dir.path().to_path_buf(),
            SEGMENT_MS,
        );
        // The mode the replay buffer ships with, and the only difference that matters to this
        // fixture: fragmented MP4 on the child's stdout instead of files in a scratch directory.
        encode.output = EncodeOutput::FragmentedStream;

        let mut capture = StubCapture::new(StubConfig { width: WIDTH, height: HEIGHT, fps: FPS });
        let mut audio = StubAudio::new(AudioFormat::default());
        let mut encoder = FfmpegEncoder::spawn(&bin, &encode).expect("spawn encoder");
        let stream = encoder.take_output_stream().expect("the stream mode exposes its pipe");
        let ring = MemoryRing::start(
            stream,
            RingSetup {
                bin: bin.clone(),
                clips_dir: clips.path().to_path_buf(),
                ram_cap_bytes: 256 * 1024 * 1024,
                // The window a trigger can ask for, sized as the engine sizes it: the pre-roll,
                // the post-roll, and one segment of slack for a fragment boundary.
                cap_ms: PRE_MS + POST_MS + SEGMENT_MS,
                pre_ms: PRE_MS,
                post_ms: POST_MS,
                encoder: "libx264".to_string(),
            },
        )
        .expect("start the in-memory ring");

        // Feed the pipeline in real time, through the same `pump_once` the engine's loop
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
            localplay_recorder::pump_once(&mut pacer, &mut capture, &mut audio, &mut encoder)
                .expect("feeding the fixture's stubs");
        }

        // The ring is filled by its own reader thread, and a fragment is only pushed once its
        // `mdat` has arrived in full — so wait for the first one rather than assuming the fed
        // timeline has landed. Nothing is scanned for: there is no directory in the picture.
        let deadline = Instant::now() + Duration::from_secs(15);
        while ring.span_ms().expect("the ring is readable") == 0 {
            assert!(
                Instant::now() < deadline,
                "fixture never completed a fragment ({:?} of timeline fed)",
                PRE_FED
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        Self {
            _clips: clips,
            _segment_dir: segment_dir,
            bin,
            ring,
            capture,
            audio,
            encoder,
            pacer,
        }
    }

    /// Exactly the call the trigger makes, returning what the wait pumped.
    fn pump(&mut self, need_ms: u64, budget: Duration) -> anyhow::Result<PumpCounts> {
        // `_on` rather than the ring-specific wrapper, because this is the entry point the
        // engine's own wait uses and it takes any `MediaRing` — the microphone slot is `None`
        // here, as it is for a recording with the microphone off.
        pump_until_span_on(
            &mut self.pacer,
            &mut self.ring,
            &mut self.capture,
            &mut self.audio,
            None,
            &mut self.encoder,
            need_ms,
            budget,
        )
    }

    /// What the ring holds. A ring that cannot be read is a broken fixture, not a zero to fold
    /// into the assertions below.
    fn stats(&self) -> MemoryStats {
        self.ring.stats().expect("the ring is readable")
    }

    /// Media time the ring can prove it holds.
    fn span_ms(&self) -> u64 {
        self.ring.span_ms().expect("the ring is readable")
    }
}

#[test]
fn the_post_roll_wait_keeps_feeding_ffmpeg_so_the_span_advances() {
    let mut fx = Fixture::new();

    // One iteration of the steady-state pump the CLI loop uses and `pump_until_span`
    // shares. The stub clock has caught up with real time by now, so whether a frame is
    // due does not matter: what matters is that a pump iteration cannot fail.
    pump_once(&mut fx.pacer, &mut fx.capture, &mut fx.audio, &mut fx.encoder)
        .expect("a pump iteration must not fail");

    let before = fx.stats();
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
    let after = fx.stats();
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

/// The hotkey path, exactly as the CLI writes it: the trigger is the ledger's own
/// media-time position and the post-roll target is `post_ms` further along the same
/// clock, so the whole window is self-consistent by construction.
///
/// The bug this pins: the CLI used to take `trigger_ms` from the `CaptureClock` (wall
/// clock) and compare `trigger_ms + post_ms` against the ledger's media-time span. On
/// the 4K box the media timeline then ran at ~0.81x of the wall clock (25 segments written
/// at 0.81/s, each exactly 1.000000s of media — the grid resampling the timeline fix
/// removed), so a wall-derived target sat ~19% beyond anything the ledger could reach and
/// every press ended in `timed out ... waiting for post-roll`. Even with the clock fixed
/// the two are not interchangeable — the ledger only counts segments ffmpeg has finished —
/// so what this test pins is the property that makes the target reachable at all: trigger,
/// target and ledger are one clock. Plus the window the resulting clip actually covers.
#[test]
fn the_trigger_and_the_post_roll_share_the_ledgers_media_clock() {
    let mut fx = Fixture::new();

    // A full pre-roll first, so the window assertions below describe the ordinary press
    // (the buffer already holds `pre_ms` of media). A press before that is the existing
    // truncated-front warning path in a ring's `trigger`, not this test.
    fx.pump(PRE_MS, Duration::from_secs(30))
        .expect("top the buffer up to a full pre-roll");

    // Exactly the engine's trigger path: the trigger position comes from the ledger, in
    // media time; `need_ms` is `post_ms` further along that same timeline; the budget is
    // the wall-clock wait for it (post-roll + margin, as the CLI sizes it).
    let trigger_ms = fx.span_ms();
    assert!(trigger_ms >= PRE_MS, "sanity: the pre-roll must be fully buffered");
    let need_ms = trigger_ms + POST_MS;
    let budget = Duration::from_millis(POST_MS) + Duration::from_secs(5);

    let started = Instant::now();
    let result = fx.pump(need_ms, budget);
    let waited = started.elapsed();
    eprintln!("media-time trigger {trigger_ms}ms, need {need_ms}ms: {result:?} after {waited:?}");
    result.expect("span + post_ms on the ledger's own clock must be reachable");

    // The ledger now covers `trigger_ms + post_ms` of media — precisely the window's
    // far edge — so the trigger below cannot refuse for `PostRollUnavailable`.
    let after = fx.stats();
    assert!(
        after.span_ms >= need_ms,
        "span must cover the media-time post-roll: need {need_ms}ms, span {}ms",
        after.span_ms
    );

    // The window is `[trigger_ms - pre_ms, trigger_ms + post_ms]` in media terms, and
    // both ends are on disk now, so the splice must cover the request in full.
    let clip = fx
        .ring
        .trigger(trigger_ms, "media-time-clip")
        .expect("the media-time post-roll must be on disk once the wait returns");
    let info = MediaInfo::probe(&fx.bin, &clip.path).expect("probe the clip");
    eprintln!(
        "clip covers [{}, {}]ms of media (requested pre={PRE_MS}ms post={POST_MS}ms): \
         {}ms, {} bytes",
        trigger_ms - PRE_MS,
        need_ms,
        info.duration_ms,
        info.size_bytes
    );
    assert!(info.video.is_some(), "clip must have video");
    assert!(info.audio.is_some(), "clip must have audio");
    // To within one segment — the granularity the window is aligned to by construction
    // (segment starts are keyframes, `window::resolve` selects whole segments).
    assert!(
        info.duration_ms + SEGMENT_MS >= PRE_MS + POST_MS,
        "clip must cover the requested pre+post in media terms to within one segment: \
         {}ms for a {PRE_MS}+{POST_MS}ms window",
        info.duration_ms
    );

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
