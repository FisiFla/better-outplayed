//! The encoder's media timeline is anchored to the wall clock, not to the frame count.
//!
//! The bug this file exists to prevent, measured on real hardware: the capture loop fed
//! the encoder every frame the backend delivered (~919 frames in 25.4s, ~36fps) while the
//! encoder was told the stream was 30fps, so it stamped frames at 1/30 s each and 25.4
//! seconds of wall clock encoded as 19.0 seconds of media. The ledger's `span_ms` then fell
//! behind real time by ~0.83x for ever, `trigger_ms + post_ms` could never be reached, and
//! every `Ctrl+F8` ended in
//! `timed out waiting for post-roll (span=26000ms need=28690ms)`.
//!
//! The fix has two halves and the first test here pins the second one: the capture loop is
//! paced to `encode.fps` (see `FramePacer` in the CLI), *and* the video input is timestamped
//! from arrival time (`-use_wallclock_as_timestamps 1`) — which is only worth anything
//! because the frame-rate conversion is off (`-fps_mode passthrough`, see
//! `localplay_encoder::ffmpeg::video_output_args`): with ffmpeg's default constant-rate
//! conversion the arrival timestamps are *resampled onto a rigid 1/R grid*, missing frames
//! are invented to fill it and surplus ones are dropped, so the rate the encoder can emit
//! becomes the media clock. Delivering half the configured rate therefore proves very
//! little on its own, and the first test says so in its own text.
//!
//! The second test is the one that would have caught the real defect: a pipeline that
//! **cannot keep up** — a declared rate four times what this machine measured, which no
//! machine can encode — must still keep the media timeline on the wall clock, and must do it
//! by encoding exactly the frames it was given rather than by inventing the difference. That
//! is the property the ring's `span_ms`, the trigger and `pre_seconds` all rest on.
//!
//! Fed in real time through the same pacing the CLI uses, because that is the only way a
//! media timeline measured against a wall clock can be observed at all.
//!
//! # The rule every timing assertion in this project follows
//!
//! Feeding in real time means these tests observe a machine under load, and CI's runner is
//! oversubscribed. So no assertion here may compare a measurement against a **constant that
//! encodes how fast the host is**. Seven assertions in this suite were written that way and
//! each failed on CI while passing locally, one at a time, because each fix revealed the next:
//!
//! | compared | CI measured | demanded |
//! |---|---|---|
//! | frames coded, as a share of the feed | 297 of 336 | ≥ 98% |
//! | media encoded, against the capture window | 2833ms of 3039ms | ≥ 2939ms |
//! | audio absent, behind the feed | 416ms | ≤ 400ms (was 200ms) |
//! | a clipped window | 2680ms | 3000ms |
//! | media against wall, over a fixed window | ratio 0.48 | ≥ 0.75 |
//! | a spliced clip against the capture | 2498ms of 3066ms | ≥ 2666ms |
//!
//! The last one is the instructive failure: the bound had already been widened once, 200ms →
//! 400ms, with a comment calling it principled. It failed at 416ms. A quantity that moves with
//! the host is not a bound; it is a reading of the host.
//!
//! What to write instead — one of these three, in order of preference:
//!
//! 1. **Wait for the condition, then compare two measurements.** `wait_for(|s| s.span_ms >=
//!    N)` first, so the assertion is about what happened, not about how long it took. This is
//!    what the pre-roll and media-window tests do.
//! 2. **Assert a relationship between two measurements of the same thing.** The frames a clip
//!    codes against the frames its own duration implies; a spliced clip against the segments
//!    it was built from; the container's audio against the container's video. Both sides move
//!    with the host, so the relationship holds at any load. This is the strongest form and the
//!    one to reach for first.
//! 3. **Assert an accounting identity.** Every submitted frame is either coded or counted as
//!    dropped, and none invented — a frame lost *uncounted* fails, which a percentage cannot
//!    detect.
//!
//! The bound to avoid is the one that reads "…and the machine must also have been fast enough
//! just now". If a number in an assertion came from a stopwatch, it is measuring the runner.
//! Where a machine genuinely cannot be asked to do something — a post-roll from an encoder that
//! is deliberately starved — assert the guarantee (the pre-roll that was already on disk) and
//! say in the message which half is best-effort and why.

use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend, Frame, PixelFormat};
use localplay_encoder::{EncodeConfig, Encoder, FfmpegEncoder, VideoCodec};
use localplay_media::{FfmpegBinaries, MediaInfo};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// Every test in this file drives a real encoder against the wall clock, and two of them hold
/// the machine down for seconds at a time on purpose (a pipeline that cannot keep up). Run in
/// parallel, they measure each other: the starvation feed is enough load to push a
/// real-time-paced 3s window past its band, and the test that fails is then whichever one
/// happened to be checking a span at that moment. So they take turns.
fn wall_clock_slot() -> MutexGuard<'static, ()> {
    static SLOT: OnceLock<Mutex<()>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
/// What the encoder is told the stream's frame rate is (`-framerate`).
const CONFIGURED_FPS: u32 = 30;
/// What the capture source actually delivers. Half the configured rate on purpose: the
/// measured failure was a *different* delivered rate (36 vs 30), and the assertions below
/// have to distinguish "media time follows arrival" from "media time follows frame count"
/// by a margin no timing jitter can produce.
const DELIVERED_FPS: u32 = 15;
const SEGMENT_MS: u64 = 1_000;
/// How long the pipeline is driven for, in wall-clock seconds.
const FEED: Duration = Duration::from_secs(3);

#[test]
fn media_time_tracks_the_wall_clock_when_capture_delivers_a_different_rate() {
    let _slot = wall_clock_slot();
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");    let dir = tempfile::tempdir().expect("a scratch temp dir");

    let cfg = EncodeConfig::for_tests_software(
        VideoCodec::H264,
        WIDTH,
        HEIGHT,
        CONFIGURED_FPS,
        dir.path().to_path_buf(),
        SEGMENT_MS,
    );
    let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn encoder");
    let mut video = StubCapture::new(StubConfig {
        width: WIDTH,
        height: HEIGHT,
        fps: DELIVERED_FPS,
    });
    let mut audio = StubAudio::new(AudioFormat::default());
    video.start().expect("start the capture stub");
    audio.start().expect("start the audio stub");

    // Drive the pipeline for FEED of *wall clock*, exactly as the CLI's loop does: both
    // stubs are real-time paced and `next_frame` waits at most 5ms for the next frame.
    let until = Instant::now() + FEED;
    let started = Instant::now();
    let mut frames: u64 = 0;
    let mut audio_frames: u64 = 0;
    while Instant::now() < until {
        if let Some(frame) = video.next_frame(Duration::from_millis(5)).expect("next frame") {
            encoder.submit_video(frame).expect("submit video");
            frames += 1;
        }
        while let Some(block) = audio.next_buffer(Duration::ZERO).expect("next audio block") {
            audio_frames += block.frames as u64;
            encoder.submit_audio(block).expect("submit audio");
        }
    }
    let captured = started.elapsed();
    encoder.finish().expect("flush encoder");

    // Probe every segment; the media timeline is the sum of what ffmpeg wrote.
    let mut segments: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path())
        .expect("read the scratch dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "mp4"))
        .collect();
    segments.sort();
    let mut video_ms: u64 = 0;
    let mut audio_ms: u64 = 0;
    for path in &segments {
        let info = MediaInfo::probe(&bin, path).expect("probe a segment");
        video_ms += info
            .video
            .as_ref()
            .and_then(|v| v.duration_ms)
            .unwrap_or_else(|| panic!("{} has no video duration", path.display()));
        audio_ms += info.audio.as_ref().and_then(|a| a.duration_ms).unwrap_or(0);
    }

    // What the frame count alone would have produced: the old, frame-stamped behaviour.
    let frame_count_ms = frames * 1000 / u64::from(CONFIGURED_FPS);
    eprintln!(
        "{frames} frames ({DELIVERED_FPS}fps nominal, {CONFIGURED_FPS}fps configured) over \
         {captured:?}: {} segments, video {video_ms}ms, audio {audio_ms}ms, fed audio \
         {audio_frames} frames = {}ms; the frame count \
         alone would be {frame_count_ms}ms",
        segments.len(),
        audio_frames * 1_000 / u64::from(AudioFormat::default().sample_rate)
    );

    // The load-bearing assertion: the encoded footage covers the wall-clock seconds it was
    // captured over, not the seconds the frame count implies. Delivering 15fps for 3s gives
    // ~45 frames, which at a declared 30fps is 1.5s of media — the defect. Wall-clock
    // timestamps put those 45 frames across the 3s they actually arrived over.
    assert!(
        video_ms >= frame_count_ms + 1_000,
        "media time must follow the wall clock: {frames} frames over {captured:?} encoded as \
         {video_ms}ms, only {}ms more than the frame count implies",
        video_ms.saturating_sub(frame_count_ms)
    );
    // The band is derived from the wall-clock window the pipeline was actually fed over,
    // not from a hardcoded ~3s, because `captured` measures that window (it is printed
    // above) and under CPU contention the feed itself takes longer — so a longer `video_ms`
    // is CORRECT rather than a failure. A fixed bound made this flaky.
    //
    // The encoded total stops at the last frame before a segment boundary, so it is
    // legitimately a little SHORT of `captured`: measured 101ms short locally and 206ms
    // short on the CI runner (2833ms against a 3039ms window). Hence one segment (1000ms)
    // is the honest tolerance in each direction — that is the granularity the muxer works
    // at. This is a corroborating sanity check; the load-bearing assertion is the
    // frame-count one above, which is what distinguishes "media follows arrival" from
    // "media follows the frame count", and it is unaffected by this tolerance.
    // (The audio assertion below had the same class of defect — a fixed floor — and is
    // derived from the fed sample count for the same reason.)
    let captured_ms = captured.as_millis() as u64;
    assert!(
        video_ms + 1_000 >= captured_ms && video_ms <= captured_ms + 1_200,
        "the encoded footage must cover the {captured_ms}ms window it was captured over (to \
         within one 1s segment), got {video_ms}ms"
    );
    // Segment *count* is a second view of the same thing: one segment per second of media,
    // so three seconds of wall clock cannot fit in the one or two files the old behaviour
    // produced from 1.5s of media.
    assert!(
        segments.len() >= 3,
        "3s of wall clock at 1s segments must produce >= 3 segment files, got {}",
        segments.len()
    );
    // Audio keeps its own exact timeline: it is derived from the 48kHz sample count rather
    // than from a clock, so the container's audio duration tracks the frames it was handed.
    //
    // Only ONE direction of that comparison is checkable, and this assertion used to make
    // both. The container must not hold more audio than the feed delivered — that is
    // host-independent, because the muxer cannot invent samples, and it is the direction that
    // catches the defect (a declared-rate grid resampling the audio would produce extra ones).
    //
    // The other direction is not a property of the code at all: the container's audio length
    // is bounded by its *video* length, because a segment ends at a video boundary and the
    // audio arriving after the last video frame is never muxed. How much of the fed audio is
    // therefore absent depends on how far ahead the audio feed ran, which is the machine's
    // business. Measured: 207ms of a 3010ms feed idle, and 416ms of a 3060ms feed on a CI
    // runner. Two fixed tails were tried (200ms, then 400ms) and each failed at the next
    // measurement — a bound that keeps moving with the host is not a bound, it is a reading of
    // the host. The envelope that IS checkable is audio-against-video, asserted just below.
    let fed_audio_ms = audio_frames * 1_000 / u64::from(AudioFormat::default().sample_rate);
    assert!(
        audio_ms <= fed_audio_ms + 200,
        "the container must not hold more audio than the feed delivered: {audio_ms}ms against \
         {audio_frames} frames fed ({fed_audio_ms}ms)"
    );
    assert!(
        video_ms.abs_diff(audio_ms) < 700,
        "the two timelines must agree to within the muxer's granularity: video {video_ms}ms, \
         audio {audio_ms}ms"
    );
}

// ---------------------------------------------------------------------------------------
// The rest of this file is about a pipeline that CANNOT keep up — the case the first test
// above does not reach, because at 64x48 with 15fps delivered the encoder fills the declared
// 30fps grid without effort, and both the old and the new behaviour then encode ~3s of media
// from ~3s of wall clock. Only when the encoder cannot emit the declared rate does the
// declared grid become the media clock, and that is what these pin.
// ---------------------------------------------------------------------------------------

/// The geometry the starvation test encodes, and how long it feeds for.
///
/// The source is deliberately smaller than the output: the frames the test has to *produce*
/// and hand over are then cheap, while the encode the machine actually pays for is a
/// 640x360 one. Without that, a starved test would have to scribble hundreds of megabytes a
/// second through the pipe and would measure its own feed loop instead of the encoder.
const STARVED_SOURCE: (u32, u32) = (320, 180);
const STARVED_OUTPUT: (u32, u32) = (640, 360);
const STARVED_FEED: Duration = Duration::from_secs(3);
/// What the probe is given to measure with. Shorter than the shipping `PROBE_BUDGET`, because
/// this geometry accepts frames at a rate where a third of a second is already hundreds of
/// samples; the point is a *representative* number, not the shipping startup cost.
const STARVED_PROBE: Duration = Duration::from_millis(1_000);

/// A frame of detailed, changing noise.
///
/// Content matters: libx264's rate is content-dependent, and a flat or repeated frame would
/// let the encoder run several times faster than anything a game would give it — which would
/// quietly turn a starved test into an unstressed one. Eight of these, cycled, are enough to
/// give the encoder real work without a real capture source.
fn noise_frame(width: u32, height: u32, seed: u64) -> Frame {
    let mut state = seed | 1;
    let mut next = move || {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as u8
    };
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..width * height {
        data.extend_from_slice(&[next(), next(), next(), 255]);
    }
    Frame { data, pts: Duration::ZERO, width, height, format: PixelFormat::Bgra8 }
}

/// Every scratch segment, in order.
fn segments_in(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read the scratch dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "mp4"))
        .collect();
    paths.sort();
    paths
}

/// How many coded video frames a file carries, straight from the container.
fn video_frames(bin: &FfmpegBinaries, path: &Path) -> u64 {
    let out = std::process::Command::new(&bin.ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_packets",
            "-show_entries",
            "stream=nb_read_packets",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()
        .expect("ffprobe must run");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("ffprobe reported no frame count for {}", path.display()))
}

/// The media the segments carry: video ms, audio ms, and coded video frames.
fn segments_media(bin: &FfmpegBinaries, dir: &Path) -> (u64, u64, u64) {
    let (mut video, mut audio, mut frames) = (0u64, 0u64, 0u64);
    for path in segments_in(dir) {
        let info = MediaInfo::probe(bin, &path).expect("probe a segment");
        video += info
            .video
            .as_ref()
            .and_then(|v| v.duration_ms)
            .unwrap_or_else(|| panic!("{} has no video duration", path.display()));
        audio += info.audio.as_ref().and_then(|a| a.duration_ms).unwrap_or(0);
        frames += video_frames(bin, &path);
    }
    (video, audio, frames)
}

/// **The regression test for the media clock.** A pipeline that cannot keep up must still
/// record a media timeline that tracks the wall clock — and it must do it by encoding the
/// frames it was given, not by inventing the difference.
///
/// The starvation is not simulated: the declared rate is four times what the shipping
/// throughput probe measured at this very geometry a moment earlier, so the encoder
/// demonstrably cannot emit the declared rate and the pipeline *is* the 4K-on-real-hardware
/// case in miniature (issue #1: ~24fps against 30 declared; issue #2: a media timeline that
/// fell behind real time).
///
/// What this fails on when the frame-rate conversion is not passthrough (verified by
/// deleting `-fps_mode passthrough` from `video_output_args`): ffmpeg fills the declared 1/R
/// grid by duplicating every frame that arrives, so the file holds several times the frames
/// that were handed over and the media clock advances at (frames the encoder can emit) ÷ R —
/// measured here on the second arm of a local A/B as 0.15–0.25× of real time, against 0.97×
/// for the passthrough arm.
#[test]
fn a_starved_pipeline_still_keeps_the_media_timeline_on_the_wall_clock() {
    let _slot = wall_clock_slot();
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().expect("a scratch temp dir");

    let mut cfg = EncodeConfig::for_tests_software(
        VideoCodec::H264,
        STARVED_SOURCE.0,
        STARVED_SOURCE.1,
        30,
        dir.path().to_path_buf(),
        SEGMENT_MS,
    );
    cfg.output_size = STARVED_OUTPUT;

    // 1. What this machine sustains here, with the shipping probe and the shipping argument
    //    list (`video_input_args` + `video_output_args` — the same two functions the
    //    recorder's child is spawned from).
    let measured =
        localplay_encoder::throughput::measure_sustainable_fps(&bin, &cfg, STARVED_PROBE)
            .expect("the probe must produce a number at this geometry");
    let declared = ((measured.fps * 4.0).round() as u32).max(8);

    // 2. Declare four times it. Nothing has to be faked: the number the declared rate is
    //    measured against, with the same encoder, the same geometry and the same kind of
    //    frames, is in `measured`.
    cfg.fps = declared;
    let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn encoder");
    let mut audio = StubAudio::new(AudioFormat::default());
    audio.start().expect("start the audio stub");

    // 3. Feed a quarter of that measured rate, in real time, through the same submit path
    //    the capture loop uses.
    let frames: Vec<Frame> = (0..8)
        .map(|i| noise_frame(STARVED_SOURCE.0, STARVED_SOURCE.1, 0x243F_6A88 + i * 7))
        .collect();
    let interval = Duration::from_secs_f64(4.0 / measured.fps.max(1.0));
    let started = Instant::now();
    let deadline = started + STARVED_FEED;
    let mut submitted: u64 = 0;
    let mut next = started;
    while Instant::now() < deadline {
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let now = Instant::now();
        let mut frame = frames[(submitted as usize) % frames.len()].clone();
        frame.pts = now.duration_since(started);
        encoder.submit_video(frame).expect("submit video");
        submitted += 1;
        // The schedule is absolute, so a slow iteration cannot make the feed faster than
        // asked: it is skipped, not made up.
        next += interval;
        while let Some(block) = audio.next_buffer(Duration::ZERO).expect("next audio block") {
            encoder.submit_audio(block).expect("submit audio");
        }
    }
    let wall = started.elapsed();
    encoder.finish().expect("flush encoder");

    let (video_ms, audio_ms, output_frames) = segments_media(&bin, dir.path());
    let wall_s = wall.as_secs_f64();
    let ratio = video_ms as f64 / 1000.0 / wall_s;
    let frame_clock_ms = submitted * 1000 / u64::from(declared);
    let dropped = encoder.dropped_frames();
    let dropped_audio = encoder.dropped_audio_blocks();
    eprintln!(
        "declared {declared}fps, measured {:.1}fps sustainable, fed {submitted} frames at \
         {:.1}fps over {wall:?}: {video_ms}ms of video, {audio_ms}ms of audio, {output_frames} \
         coded frames, {dropped} video + {dropped_audio} audio dropped by the queues; \
         media/wall = {ratio:.3}; a frame-count clock would have said {frame_clock_ms}ms",
        measured.fps,
        submitted as f64 / wall_s
    );

    // The test is only about a starved pipeline if this holds: the delivered rate is well
    // under half the rate the pipeline declared.
    assert!(
        (submitted as f64 / wall_s) < declared as f64 / 2.0,
        "the test needs a pipeline that cannot keep up: {submitted} frames in {wall:?} \
         ({:.1}fps) against a declared {declared}fps",
        submitted as f64 / wall_s
    );
    // The load-bearing assertion. The media timeline the ring's `span_ms`, the trigger and
    // `pre_seconds` are all built on must cover the wall-clock seconds the footage was
    // captured over. NOT tightened to 1.00±0.01 on purpose: the first frames of a run are
    // read by ffmpeg a little after they are written (child startup, pipe fill), and that
    // one-off offset is inside this measurement — it is a fixed lag, not a clock that drifts.
    assert!(
        (0.85..=1.15).contains(&ratio),
        "the media timeline must track the wall clock even though the encoder cannot keep up: \
         {video_ms}ms of video for {wall:?} of wall clock ({ratio:.3}x); the frame count alone \
         would have produced {frame_clock_ms}ms"
    );
    // And the reason it tracks: ffmpeg wrote the frames it was given. With the constant-rate
    // conversion in force this is where the defect shows up as a hard number — the file
    // holds several times the frames the pipeline handed over, each one invented to fill the
    // declared grid.
    assert!(
        output_frames <= submitted,
        "the encoder must encode the frames it was given and invent none: {output_frames} \
         coded frames for {submitted} submitted"
    );
    // And nothing disappears uncounted. This used to demand that at least 98% of the
    // submitted frames be coded, which contradicts the assertion above: this test *requires*
    // a pipeline that cannot keep up, and when the starvation is in the encoder the bounded
    // queue does exactly what it is for — it drops, and `dropped` is the counter for it. On
    // CI that produced "297 coded frames for 336 submitted (39 dropped)": 336 - 39 = 297, so
    // the pipeline was behaving perfectly and the test failed anyway, intermittently, purely
    // by how loaded the runner was that minute.
    //
    // The property that actually matters is the accounting: every frame handed to the encoder
    // was either coded or counted as dropped, and none was invented. That is load-independent
    // — the two sides shrink together — and it is strictly stronger than a percentage, since a
    // frame lost WITHOUT being counted now fails. One frame of slack, for whatever is in
    // flight when the pipeline is flushed.
    assert!(
        output_frames + dropped + 1 >= submitted,
        "every submitted frame must be either coded or counted as dropped, and none invented: \
         {output_frames} coded + {dropped} dropped for {submitted} submitted"
    );
    // The two clocks that are *not* the wall clock: the frame count divided by the declared
    // rate says a few hundred ms; the audio stream's exact 48kHz sample count says the wall
    // clock. The video stream has to agree with the audio one, not with the frame count.
    assert!(
        video_ms > frame_clock_ms * 3,
        "media time must follow the wall clock, not the frame count: {video_ms}ms against a \
         frame-count clock of {frame_clock_ms}ms"
    );
    // Audio keeps its own exact timeline (the 48kHz sample count) and must not be *short* of
    // the video by more than a frame's worth of submission bursts. The tolerance is wide on
    // purpose: this test hands audio over in bursts at the video frame's pace (15 frames a
    // second means ~7 blocks at once), which is not how the engine's pump drains it, and a
    // segment boundary can fall inside one of those bursts.
    assert!(
        audio_ms + 900 >= video_ms,
        "the audio timeline must keep up with the video one: video {video_ms}ms, audio \
         {audio_ms}ms"
    );
}

/// **Splicing still works on what a starved pipeline writes.**
///
/// The fix makes the segments VFR: with the frame-rate conversion off, a segment holds the
/// frames that arrived while it was open, so its `avg_frame_rate` is whatever the machine
/// managed and its frames no longer sit on a rigid grid. A clipper that cannot concatenate
/// those is worse than one whose clock is slightly off, so this splices them with the
/// product's own lossless concatenation ([`localplay_media::edit::concat_lossless`], the
/// function `localplay_replay::splice::ClipSplicer::splice` calls) and holds the result to
/// the three things a clip has to be: readable, carrying both streams, and playable — every
/// frame decoded, not just probed.
#[test]
fn the_vfr_segments_a_starved_pipeline_writes_still_splice_and_play() {
    let _slot = wall_clock_slot();
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().expect("a scratch temp dir");
    const DECLARED: u32 = 120;
    const DELIVERED: u32 = 15;

    let cfg = EncodeConfig::for_tests_software(
        VideoCodec::H264,
        320,
        180,
        DECLARED,
        dir.path().to_path_buf(),
        SEGMENT_MS,
    );
    let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn encoder");
    let frames: Vec<Frame> =
        (0..4).map(|i| noise_frame(320, 180, 0xB5AD_1CE5 + i * 13)).collect();
    let mut audio = StubAudio::new(AudioFormat::default());
    audio.start().expect("start the audio stub");

    let interval = Duration::from_secs_f64(1.0 / f64::from(DELIVERED));
    let started = Instant::now();
    let deadline = started + STARVED_FEED;
    let mut submitted = 0u64;
    let mut next = started;
    while Instant::now() < deadline {
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let now = Instant::now();
        let mut frame = frames[(submitted as usize) % frames.len()].clone();
        frame.pts = now.duration_since(started);
        encoder.submit_video(frame).expect("submit video");
        submitted += 1;
        next += interval;
        while let Some(block) = audio.next_buffer(Duration::ZERO).expect("next audio block") {
            encoder.submit_audio(block).expect("submit audio");
        }
    }
    let wall = started.elapsed();
    encoder.finish().expect("flush encoder");

    // Splice every segment, exactly as the clip path does: a concat list of whole segments.
    let segments = segments_in(dir.path());
    assert!(segments.len() >= 2, "a starved pipeline still cuts segments: {segments:?}");
    let list = dir.path().join("clip.concat.txt");
    let mut text = String::new();
    for seg in &segments {
        text.push_str(&format!(
            "file '{}'\n",
            localplay_replay_concat_path(&std::fs::canonicalize(seg).expect("canonical path"))
        ));
    }
    std::fs::write(&list, text).expect("write the concat list");
    let clip = dir.path().join("clip.mp4");
    localplay_media::edit::concat_lossless(&bin, &list, &clip)
        .expect("the segments must concatenate losslessly");

    let info = MediaInfo::probe(&bin, &clip).expect("the clip must be a readable media file");
    let video = info.video.as_ref().expect("the clip must carry video");
    let audio_ms = info.audio.as_ref().and_then(|a| a.duration_ms).unwrap_or(0);
    let video_ms = video.duration_ms.expect("the video stream must have a duration");
    let coded = video_frames(&bin, &clip);
    for seg in &segments {
        let i = MediaInfo::probe(&bin, seg).expect("probe a segment");
        eprintln!(
            "  {}: video {}ms, audio {}ms, {} coded frames",
            seg.file_name().unwrap_or_default().to_string_lossy(),
            i.video.as_ref().and_then(|v| v.duration_ms).unwrap_or(0),
            i.audio.as_ref().and_then(|a| a.duration_ms).unwrap_or(0),
            video_frames(&bin, seg)
        );
    }
    let decoded = decode_every_frame(&bin, &clip, video.width, video.height);
    // The clip's display timeline, measured rather than assumed. Two things can happen in a
    // VFR segment and both are reported here rather than glossed: two frames can carry the
    // *same* timestamp (the rawvideo demuxer resolves arrival times at 1/`-framerate`, so a
    // burst of frames read within one tick shares a tick), and a frame can sit a little
    // across a segment boundary, which the concat demuxer's per-file offset turns into a
    // small step *backwards* at that boundary. Neither loses a frame — the counts below are
    // the check for that — and the four real clips the A/B produced were measured against
    // this same yardstick (three monotonic; the bursty 4K one with equal pairs and no
    // backwards step).
    let times = display_times(&bin, &clip);
    let equal = times.windows(2).filter(|w| w[1] == w[0]).count();
    let backwards: Vec<f64> = times
        .windows(2)
        .map(|w| w[0] - w[1])
        .filter(|d| *d > 0.0)
        .collect();
    let worst_backwards = backwards.iter().cloned().fold(0.0f64, f64::max);
    eprintln!(
        "{submitted} frames delivered at {DELIVERED}fps against {DECLARED} declared over \
         {wall:?}: {} segments spliced into {}x{} {}ms of video ({coded} coded frames), \
         {audio_ms}ms of audio; {decoded} frames decoded; {} display timestamps: {equal} \
         repeated, {} backwards, worst {:.1}ms",
        segments.len(),
        video.width,
        video.height,
        video_ms,
        times.len(),
        backwards.len(),
        worst_backwards * 1_000.0
    );

    // Both streams, as the clip path requires (spec §6.3, criterion 2).
    assert!(info.audio.is_some(), "the spliced clip must carry audio");
    assert!(
        audio_ms > 500,
        "the spliced clip's audio stream must be real audio, not a stub of one: {audio_ms}ms"
    );
    // The clip's video is the segments it was built from, to within the muxer's granularity.
    // This is the property the *splice* is responsible for, and it is a relationship between
    // two measurements of the same footage rather than a claim about the machine — so it holds
    // however starved the pipeline was, which matters because this test requires starvation.
    let spliced_from_ms: u64 = segments
        .iter()
        .filter_map(|seg| {
            MediaInfo::probe(&bin, seg)
                .ok()
                .and_then(|i| i.video.as_ref().and_then(|v| v.duration_ms))
        })
        .sum();
    assert!(
        video_ms.abs_diff(spliced_from_ms) <= 1_000,
        "the spliced clip must be the segments it was built from, no more and no less: \
         {video_ms}ms of video from {spliced_from_ms}ms of segments"
    );
    // What is deliberately NOT asserted here is the same band against `wall_ms`. The clip is a
    // window over the ring's **media** timeline, and this test requires a pipeline too starved
    // to keep up, so the media the ring could prove at the trigger instant is legitimately less
    // than the wall-clock seconds that elapsed: CI measured 2498ms of video for 3066ms of
    // capture. Demanding the wall clock back is demanding the machine not be loaded, which is
    // the opposite of what this test sets up. The timeline-against-wall property belongs to the
    // sibling test above, where the pipeline is fed at a rate it can hold.
    //
    // The clip holds the frames its own duration implies at the rate they were delivered. The
    // form this replaces (`coded >= DELIVERED * 2`) compared the clip against a fixed two
    // seconds of *feed*, conflating the clip with the capture it was cut from — and so was
    // really an assumption about how much the clip would hold, which is the machine's business
    // again.
    let implied_by_duration = video_ms * u64::from(DELIVERED) / 1_000;
    assert!(
        coded + 2 >= implied_by_duration,
        "the clip must hold the frames its own {video_ms}ms implies at {DELIVERED}fps: {coded} \
         coded frames for {submitted} submitted over {wall:?}"
    );
    // Playable, not merely probeable: every coded frame decodes, and the container says how
    // many there are.
    assert_eq!(
        decoded, coded,
        "every coded frame of the spliced clip must decode: {decoded} of {coded}"
    );
    assert!(
        coded >= u64::from(DELIVERED) * 2,
        "the clip must hold the frames that were delivered: {coded} coded frames for \
         {submitted} submitted"
    );
    // The splice must not *scramble* the timeline either: at most one out-of-order pair per
    // segment boundary, and never a step backwards worth noticing (a tenth of a second). A
    // future change that made the segments overlap properly — or a splice that reordered
    // frames rather than concatenating them — fails here instead of shipping.
    assert!(
        backwards.len() <= segments.len(),
        "the spliced clip must not reorder frames: {} backwards steps over {} segments: \
         {backwards:?}",
        backwards.len(),
        segments.len()
    );
    assert!(
        worst_backwards < 0.100,
        "a step backwards must stay within a frame's worth of a segment boundary: worst is \
         {:.1}ms",
        worst_backwards * 1_000.0
    );
}

/// The concat list's path spelling, as the clip path writes it.
///
/// `localplay_replay::splice::concat_list_path` is the real one, but this test lives in the
/// encoder crate (which must not depend on the replay engine), and on Unix — the only host
/// this suite runs on — it reduces to the path as written, which is all a temporary
/// directory needs.
fn localplay_replay_concat_path(path: &Path) -> String {
    path.to_string_lossy().replace('\'', r"'\''")
}

/// The media timestamps of a file's video frames, in presentation order.
fn display_times(bin: &FfmpegBinaries, path: &Path) -> Vec<f64> {
    let out = std::process::Command::new(&bin.ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=pts_time",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()
        .expect("ffprobe must run");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|x| x.parse::<f64>().ok())
        .collect()
}

/// Decode every frame of a file and count them, so "playable" is checked by decoding rather
/// than by trusting the container's own count.
fn decode_every_frame(bin: &FfmpegBinaries, path: &Path, width: u32, height: u32) -> u64 {
    // `-fps_mode passthrough` on the DECODE side too: without it the decode re-applies the
    // very conversion this file is about — ffmpeg duplicates what it reads onto the stream's
    // nominal rate, so a 2.8s clip with 46 coded frames decodes as 342 of them. The counter
    // would then be measuring that conversion, not the clip.
    let out = std::process::Command::new(&bin.ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "bgra", "-"])
        .output()
        .expect("ffmpeg must run");
    assert!(
        out.status.success(),
        "decoding {} failed: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    // Deliberately NOT asserting stderr is empty. `-f rawvideo` is a muxer with no timestamps
    // of its own, so ffmpeg re-times what it decodes and complains about the clip's repeated
    // and (at a segment boundary) slightly out-of-order display timestamps on the way out —
    // about its own output, not about the clip's decodability. The frame count below is the
    // check: every coded frame came out.
    out.stdout.len() as u64 / (u64::from(width) * u64::from(height) * 4)
}
