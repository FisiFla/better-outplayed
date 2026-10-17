//! Regression test for the hotkey's post-roll wait.
//!
//! The trigger path used to wait for the post-roll with a loop that only slept and re-scanned
//! the scratch directory, never calling `capture.next_frame` / `encoder.submit_video` /
//! `submit_audio`. ffmpeg received no data, produced no new segments, `span_ms` could not
//! advance, and every `Ctrl+F8` press ended in `timed out waiting for post-roll`. Nothing caught
//! it because the hotkey cannot fire on the macOS development host.
//!
//! The hotkey still needs Windows. What is tested here is the wait it drives, in isolation:
//! `localplay_recorder::pump_until_span_on` over stub capture sources, a real ffmpeg encoder and
//! the in-memory ring the replay buffer ships with. A fragment carries its own length, so that
//! ring's span is exact instead of lagging a segment behind.
//!
//! Gating: deliberately not `#![cfg(feature = "test-encoders")]`. The suite enables that
//! feature on the encoder package, not this one, so a `cfg` gate here would run an empty test
//! binary and silently drop the regression test (`running 0 tests`). `localplay-encoder` is
//! instead a dev-dependency of this package with `test-encoders` on (`Cargo.toml`), which makes
//! `EncodeConfig::for_tests_software` exist in every test build and no shipping one.
//!
//! Needs ffmpeg on `PATH` with a working libx264.

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
/// Wall-clock time: the encoder's media timeline is the wall clock (see the module comment in
/// `localplay_encoder::ffmpeg`), so the fixture feeds its stubs in real time, through the same
/// `pump_once` the CLI uses. The same frames in a burst would produce almost no footage.
const PRE_FED: Duration = Duration::from_secs(3);

/// A live pipeline over stub sources: capture, encoder, in-memory ring.
///
/// Owns its temp directory, so it outlives the clip the pipeline writes into it.
struct Fixture {
    _clips: tempfile::TempDir,
    /// Never written to. `EncodeConfig::for_tests_software` asks for a segment directory, but this
    /// pipeline runs in `FragmentedStream` mode: ffmpeg writes to its stdout and no per-second file
    /// is produced.
    _segment_dir: tempfile::TempDir,
    bin: FfmpegBinaries,
    ring: MemoryRing,
    capture: StubCapture,
    audio: StubAudio,
    encoder: FfmpegEncoder,
    /// The same rate limit the CLI's own loop applies, at the stub's frame rate, so the wait is
    /// driven exactly as the CLI drives it.
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
        // The mode the replay buffer ships with: fragmented MP4 on the child's stdout instead of files
        // in a scratch directory.
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
                // The window a trigger can ask for, sized as the engine sizes it: pre-roll, post-roll, and one
                // segment of slack for a fragment boundary.
                cap_ms: PRE_MS + POST_MS + SEGMENT_MS,
                pre_ms: PRE_MS,
                post_ms: POST_MS,
                encoder: "libx264".to_string(),
            },
        )
        .expect("start the in-memory ring");

        // Feed the pipeline in real time, through the same `pump_once` the engine's loop and the
        // post-roll wait use. The media timeline comes from the wall clock, so an unpaced burst lands
        // within a few milliseconds of capture time and encodes as a few milliseconds of media,
        // useless for a fixture that needs seconds of buffer.
        capture.start().unwrap();
        audio.start().unwrap();
        let mut pacer = FramePacer::new(FPS);
        let feed_until = Instant::now() + PRE_FED;
        while Instant::now() < feed_until {
            localplay_recorder::pump_once(&mut pacer, &mut capture, Some(&mut audio), &mut encoder)
                .expect("feeding the fixture's stubs");
        }

        // The ring is filled by its own reader thread, and a fragment is pushed only once its `mdat`
        // has fully arrived, so wait for the first one rather than assuming the fed timeline landed.
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
        // `_on` rather than the ring-specific wrapper: it is the entry point the engine's own wait uses
        // and takes any `MediaRing`. The microphone slot is `None`, as for a recording with it off.
        pump_until_span_on(
            &mut self.pacer,
            &mut self.ring,
            &mut self.capture,
            Some(&mut self.audio),
            None,
            &mut self.encoder,
            need_ms,
            budget,
        )
    }

    /// What the ring holds. A ring that cannot be read is a broken fixture, not a zero to fold into
    /// the assertions.
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

    // One iteration of the steady-state pump the CLI loop uses and `pump_until_span` shares. The
    // stub clock has caught up with real time by now, so whether a frame is due does not matter;
    // what matters is that a pump iteration cannot fail.
    pump_once(&mut fx.pacer, &mut fx.capture, Some(&mut fx.audio), &mut fx.encoder)
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

    // The load-bearing assertion: the span only moves when ffmpeg finalises a segment, and ffmpeg
    // only does so while frames keep arriving. A wait that does not pump cannot satisfy this,
    // however long its budget.
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
    // Sanity check that the wait really ran rather than skipping ahead. Media time and wall time
    // are equal only at the capture source: ffmpeg holds frames in its own lookahead and in the
    // pipe (measured: the wait returned after 1.10s having covered 2000ms of media), so a wait may
    // legitimately finish sooner than `post_ms` of wall clock. "The media covering need_ms is held"
    // is asserted above and is what fails when the wait does not pump; the floor below only says
    // the wait was not instantaneous.
    assert!(
        waited >= Duration::from_millis(100),
        "the wait must take real time at all; it returned after {waited:?}"
    );

    // What the wait is for. The next step, `ring.trigger(trigger_ms, ..)`, refuses to splice until
    // the ledger covers `trigger_ms + post_ms` (`WindowError::PostRollUnavailable`, seen by a user
    // as "no clip was ever written"). So the wait is correct only if the trigger it precedes
    // succeeds and yields a clip with both streams. `need_ms` is exactly `trigger_ms + post_ms`.
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

/// The hotkey path as the CLI writes it: the trigger is the ledger's own media-time position and
/// the post-roll target is `post_ms` further along the same clock, so the window is
/// self-consistent by construction.
///
/// Pins a bug where the CLI took `trigger_ms` from the wall-clock `CaptureClock` and compared
/// `trigger_ms + post_ms` against the ledger's media-time span. On a 4K machine the media
/// timeline then ran at ~0.81x of wall clock, so a wall-derived target sat ~19% beyond anything
/// the ledger could reach and every press ended in `timed out ... waiting for post-roll`. Even
/// with the clocks fixed they are not interchangeable (the ledger counts only finished
/// segments), so this pins that trigger, target and ledger are one clock, plus the window the
/// clip covers.
#[test]
fn the_trigger_and_the_post_roll_share_the_ledgers_media_clock() {
    let mut fx = Fixture::new();

    // A full pre-roll first, so the window assertions describe the ordinary press. A press before
    // that is the truncated-front warning path in a ring's `trigger`, not this test.
    fx.pump(PRE_MS, Duration::from_secs(30))
        .expect("top the buffer up to a full pre-roll");

    // The engine's trigger path: the trigger position comes from the ledger in media time;
    // `need_ms` is `post_ms` further along that timeline; the budget is the wall-clock wait for it
    // (post-roll + margin, as the CLI sizes it).
    let trigger_ms = fx.span_ms();
    assert!(trigger_ms >= PRE_MS, "sanity: the pre-roll must be fully buffered");
    let need_ms = trigger_ms + POST_MS;
    let budget = Duration::from_millis(POST_MS) + Duration::from_secs(5);

    let started = Instant::now();
    let result = fx.pump(need_ms, budget);
    let waited = started.elapsed();
    eprintln!("media-time trigger {trigger_ms}ms, need {need_ms}ms: {result:?} after {waited:?}");
    result.expect("span + post_ms on the ledger's own clock must be reachable");

    // The ledger now covers `trigger_ms + post_ms` of media, the window's far edge, so the trigger
    // below cannot refuse for `PostRollUnavailable`.
    let after = fx.stats();
    assert!(
        after.span_ms >= need_ms,
        "span must cover the media-time post-roll: need {need_ms}ms, span {}ms",
        after.span_ms
    );

    // The window is `[trigger_ms - pre_ms, trigger_ms + post_ms]` in media terms and both ends are
    // held, so the splice must cover the request in full.
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
    // To within one segment, the granularity the window is aligned to (segment starts are
    // keyframes, `window::resolve` selects whole segments).
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
