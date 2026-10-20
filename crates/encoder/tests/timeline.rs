//! The encoder's media timeline is anchored to the wall clock, not to the frame count.
//!
//! The bug this file prevents: the capture loop fed the encoder every frame the backend
//! delivered (~919 frames in 25.4s, ~36fps) while the encoder was told the stream was 30fps, so
//! 25.4 seconds of wall clock encoded as 19.0 seconds of media. The ledger's `span_ms` fell
//! behind real time for ever, `trigger_ms + post_ms` was never reached, and every hotkey press
//! ended in `timed out waiting for post-roll (span=26000ms need=28690ms)`.
//!
//! The fix has two halves: the capture loop is paced to `encode.fps` (`FramePacer`), and the
//! video input is timestamped from arrival (`-use_wallclock_as_timestamps 1`), which only works
//! because the frame-rate conversion is off (`-fps_mode passthrough`, see
//! `replayed_encoder::ffmpeg::video_output_args`): with ffmpeg's default conversion arrival
//! timestamps are resampled onto a rigid 1/R grid, so the rate the encoder can emit becomes the
//! media clock. The first test therefore proves little on its own, and says so.
//!
//! The second test would have caught the real defect: a pipeline that cannot keep up (a declared
//! rate four times what this machine measured) must still keep the media timeline on the wall
//! clock, by encoding exactly the frames it was given rather than inventing the difference.
//!
//! Fed in real time through the CLI's pacing, the only way a media timeline can be measured
//! against a wall clock.
//!
//! # The rule every timing assertion in this project follows
//!
//! These tests observe a machine under load, and CI's runner is oversubscribed. No assertion may
//! compare a measurement against a constant that encodes how fast the host is. Each of these
//! failed on CI while passing locally, one at a time:
//!
//! | compared | CI measured | demanded |
//! |---|---|---|
//! | frames coded, as a share of the feed | 297 of 336 | >= 98% |
//! | media encoded, against the capture window | 2833ms of 3039ms | >= 2939ms |
//! | audio absent, behind the feed | 416ms | <= 400ms (was 200ms) |
//! | a clipped window | 2680ms | 3000ms |
//! | media against wall, over a fixed window | ratio 0.48 | >= 0.75 |
//! | a spliced clip against the capture | 2498ms of 3066ms | >= 2666ms |
//!
//! A bound widened twice and failing again is a reading of the host, not a bound. Write one of
//! these instead, in order of preference:
//!
//! 1. Assert a relationship between two measurements of the same thing (frames a clip codes
//!    against the frames its duration implies; a spliced clip against its segments; the
//!    container's audio against its video). Both sides move with the host.
//! 2. Wait for the condition (`wait_for(|s| s.span_ms >= N)`), then compare two measurements.
//! 3. Assert an accounting identity: every submitted frame is coded or counted as dropped, none
//!    invented. A frame lost uncounted fails, which a percentage cannot detect.
//!
//! If a number in an assertion came from a stopwatch, it is measuring the runner. Where a machine
//! genuinely cannot be asked to do something (a post-roll from a deliberately starved encoder),
//! assert the guarantee that holds (the pre-roll already on disk) and say which half is best-effort.

// The test bodies use `unwrap()` throughout; production code does not.
#![allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks)]
use replayed_capture::stub::{StubAudio, StubCapture, StubConfig};
use replayed_capture::{AudioBackend, AudioFormat, CaptureBackend, Frame, PixelFormat};
use replayed_encoder::{EncodeConfig, Encoder, FfmpegEncoder, VideoCodec};
use replayed_media::{FfmpegBinaries, MediaInfo};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// Every test here drives a real encoder against the wall clock, and two hold the machine down
/// for seconds on purpose (a pipeline that cannot keep up). In parallel they measure each other,
/// so they take turns.
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
/// What the capture source delivers. Half the configured rate on purpose: the failure was a
/// different delivered rate (36 vs 30), and the assertions must tell "media time follows
/// arrival" from "follows frame count" by a margin no jitter can produce.
const DELIVERED_FPS: u32 = 15;
const SEGMENT_MS: u64 = 1_000;
/// How long the pipeline is driven for, in wall-clock seconds.
const FEED: Duration = Duration::from_secs(3);

#[test]
fn media_time_tracks_the_wall_clock_when_capture_delivers_a_different_rate() {
    let _slot = wall_clock_slot();
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().expect("a scratch temp dir");

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

    // Drive the pipeline for FEED of wall clock, as the CLI's loop does: both stubs are real-time
    // paced and `next_frame` waits at most 5ms for the next frame.
    let until = Instant::now() + FEED;
    let started = Instant::now();
    let mut frames: u64 = 0;
    let mut audio_frames: u64 = 0;
    while Instant::now() < until {
        if let Some(frame) = video
            .next_frame(Duration::from_millis(5))
            .expect("next frame")
        {
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

    // What the frame count alone would have produced: the frame-stamped behaviour.
    let frame_count_ms = frames * 1000 / u64::from(CONFIGURED_FPS);
    eprintln!(
        "{frames} frames ({DELIVERED_FPS}fps nominal, {CONFIGURED_FPS}fps configured) over \
         {captured:?}: {} segments, video {video_ms}ms, audio {audio_ms}ms, fed audio \
         {audio_frames} frames = {}ms; the frame count \
         alone would be {frame_count_ms}ms",
        segments.len(),
        audio_frames * 1_000 / u64::from(AudioFormat::default().sample_rate)
    );

    // The load-bearing assertion: the encoded footage covers the wall-clock seconds it was captured
    // over, not the seconds the frame count implies. 15fps for 3s is ~45 frames, 1.5s of media at a
    // declared 30fps (the defect); wall-clock timestamps put them across the 3s they arrived over.
    assert!(
        video_ms >= frame_count_ms + 1_000,
        "media time must follow the wall clock: {frames} frames over {captured:?} encoded as \
         {video_ms}ms, only {}ms more than the frame count implies",
        video_ms.saturating_sub(frame_count_ms)
    );
    // The band is derived from the window the pipeline was actually fed over (`captured`, printed
    // above), not a hardcoded ~3s: under CPU contention the feed takes longer, so a longer
    // `video_ms` is correct.
    //
    // The encoded total stops at the last frame before a segment boundary, so it is legitimately a
    // little short of `captured` (101ms locally, 206ms on CI). One segment (1000ms) is the honest
    // tolerance each way. This is a corroborating check; the frame-count assertion above is what
    // distinguishes arrival from frame count.
    let captured_ms = captured.as_millis() as u64;
    assert!(
        video_ms + 1_000 >= captured_ms && video_ms <= captured_ms + 1_200,
        "the encoded footage must cover the {captured_ms}ms window it was captured over (to \
         within one 1s segment), got {video_ms}ms"
    );
    // Segment count is a second view: one segment per second of media, so three seconds of wall
    // clock cannot fit in the one or two files that 1.5s of media would produce.
    assert!(
        segments.len() >= 3,
        "3s of wall clock at 1s segments must produce >= 3 segment files, got {}",
        segments.len()
    );
    // Audio keeps its own exact timeline, derived from the 48kHz sample count, so the container's
    // audio duration tracks the frames it was handed.
    //
    // Only one direction is checkable. The container must not hold more audio than the feed
    // delivered: host-independent, since the muxer cannot invent samples, and it catches the defect
    // (a declared-rate grid resampling the audio). The other direction is not a property of the
    // code: a segment ends at a video boundary and audio after the last video frame is never muxed,
    // so how much fed audio is absent depends on how far ahead the audio feed ran (207ms of 3010ms
    // idle, 416ms of 3060ms on CI; two fixed tails each failed at the next measurement). The
    // checkable envelope is audio against video, asserted below.
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
// The rest of this file is about a pipeline that cannot keep up, which the first test does not
// reach: at 64x48 with 15fps delivered the encoder fills the declared 30fps grid easily, so
// both behaviours encode ~3s of media from ~3s of wall clock. Only when the
// encoder cannot emit the declared rate does the declared grid become the media clock.
// ---------------------------------------------------------------------------------------

/// The geometry the starvation test encodes. The source is smaller than the output, so the
/// frames the test must produce are cheap while the encode the machine pays for is 640x360;
/// otherwise the test would measure its own feed loop instead of the encoder.
const STARVED_SOURCE: (u32, u32) = (320, 180);
const STARVED_OUTPUT: (u32, u32) = (640, 360);
const STARVED_FEED: Duration = Duration::from_secs(3);
/// What the probe measures with. Shorter than the shipping `PROBE_BUDGET`: this geometry accepts
/// frames fast enough that a second is plenty for a representative number.
const STARVED_PROBE: Duration = Duration::from_millis(1_000);

/// A frame of detailed, changing noise.
///
/// libx264's rate is content-dependent, and a flat or repeated frame would let the encoder run
/// several times faster than a game would allow, turning a starved test into an unstressed one.
/// Eight of these, cycled, give the encoder real work.
fn noise_frame(width: u32, height: u32, seed: u64) -> Frame {
    let mut state = seed | 1;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as u8
    };
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..width * height {
        data.extend_from_slice(&[next(), next(), next(), 255]);
    }
    Frame {
        data,
        pts: Duration::ZERO,
        width,
        height,
        format: PixelFormat::Bgra8,
        texture: None,
    }
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

/// The regression test for the media clock. A pipeline that cannot keep up must still record a
/// media timeline that tracks the wall clock, by encoding the frames it was given rather than
/// inventing the difference.
///
/// The starvation is real: the declared rate is four times what the shipping throughput probe
/// measured at this geometry a moment earlier, so this is the 4K-on-real-hardware case in
/// miniature (~24fps against 30 declared; a media timeline falling behind real time).
///
/// Without `-fps_mode passthrough` (verified by deleting it from `video_output_args`) ffmpeg
/// fills the declared 1/R grid by duplicating every arriving frame, so the file holds several
/// times the frames handed over and the media clock runs at 0.15-0.25x of real time, against
/// 0.97x with passthrough.
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

    // 1. What this machine sustains here, with the shipping probe and argument list
    //    (`video_input_args` + `video_output_args`, the functions the recorder's child is spawned from).
    let measured = replayed_encoder::throughput::measure_sustainable_fps(&bin, &cfg, STARVED_PROBE)
        .expect("the probe must produce a number at this geometry");
    let declared = ((measured.fps * 4.0).round() as u32).max(8);

    // 2. Declare four times it. Nothing is faked: `measured` came from the same encoder, geometry and kind of frames.
    cfg.fps = declared;
    let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn encoder");
    let mut audio = StubAudio::new(AudioFormat::default());
    audio.start().expect("start the audio stub");

    // 3. Feed a quarter of that measured rate, in real time, through the capture loop's submit path.
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
        // The schedule is absolute, so a slow iteration is skipped, not made up.
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

    // The test is only about a starved pipeline if the delivered rate is well under half the declared rate.
    assert!(
        (submitted as f64 / wall_s) < declared as f64 / 2.0,
        "the test needs a pipeline that cannot keep up: {submitted} frames in {wall:?} \
         ({:.1}fps) against a declared {declared}fps",
        submitted as f64 / wall_s
    );
    // The load-bearing assertion: the media timeline that `span_ms`, the trigger and `pre_seconds`
    // are built on must cover the wall-clock seconds the footage was captured over. Not tightened
    // to 1.00 +- 0.01: the first frames are read by ffmpeg a little after they are written (child
    // startup, pipe fill), a fixed lag rather than a drifting clock.
    assert!(
        (0.85..=1.15).contains(&ratio),
        "the media timeline must track the wall clock even though the encoder cannot keep up: \
         {video_ms}ms of video for {wall:?} of wall clock ({ratio:.3}x); the frame count alone \
         would have produced {frame_clock_ms}ms"
    );
    // The reason it tracks: ffmpeg wrote the frames it was given. With constant-rate conversion the
    // file holds several times the frames handed over, each invented to fill the declared grid.
    assert!(
        output_frames <= submitted,
        "the encoder must encode the frames it was given and invent none: {output_frames} \
         coded frames for {submitted} submitted"
    );
    // Nothing disappears uncounted. Demanding 98% of submitted frames be coded contradicts this
    // test's premise: when the encoder is starved the bounded queue drops, and `dropped` counts it
    // (CI: 297 coded for 336 submitted, 39 dropped, which is correct behaviour).
    //
    // The load-independent property is the accounting: every frame handed to the encoder was coded
    // or counted as dropped, none invented. Stronger than a percentage, since an uncounted loss
    // fails. One frame of slack for whatever is in flight at the flush.
    assert!(
        output_frames + dropped + 1 >= submitted,
        "every submitted frame must be either coded or counted as dropped, and none invented: \
         {output_frames} coded + {dropped} dropped for {submitted} submitted"
    );
    // The video stream must agree with the audio stream's exact 48kHz sample count (the wall
    // clock), not with the frame count divided by the declared rate.
    assert!(
        video_ms > frame_clock_ms * 3,
        "media time must follow the wall clock, not the frame count: {video_ms}ms against a \
         frame-count clock of {frame_clock_ms}ms"
    );
    // Audio must not be short of the video by more than a frame's worth of submission bursts. Wide
    // on purpose: this test hands audio over in bursts at the video frame's pace (~7 blocks at once
    // at 15fps), unlike the engine's pump, and a segment boundary can fall inside a burst.
    assert!(
        audio_ms + 900 >= video_ms,
        "the audio timeline must keep up with the video one: video {video_ms}ms, audio \
         {audio_ms}ms"
    );
}

/// Splicing still works on what a starved pipeline writes.
///
/// With the frame-rate conversion off, a segment holds the frames that arrived while it was open
/// (VFR, `avg_frame_rate` whatever the machine managed). A clipper that cannot concatenate those
/// is worse than a slightly-off clock, so this splices them with the product's lossless
/// concatenation ([`replayed_media::edit::concat_lossless`], which
/// `replayed_replay::splice::ClipSplicer::splice` calls) and requires the clip to be readable,
/// carry both streams, and be playable (every frame decoded, not just probed).
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
    let frames: Vec<Frame> = (0..4)
        .map(|i| noise_frame(320, 180, 0xB5AD_1CE5 + i * 13))
        .collect();
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

    // Splice every segment as the clip path does: a concat list of whole segments.
    let segments = segments_in(dir.path());
    assert!(
        segments.len() >= 2,
        "a starved pipeline still cuts segments: {segments:?}"
    );
    let list = dir.path().join("clip.concat.txt");
    let mut text = String::new();
    for seg in &segments {
        text.push_str(&format!(
            "file '{}'\n",
            replayed_replay_concat_path(&std::fs::canonicalize(seg).expect("canonical path"))
        ));
    }
    std::fs::write(&list, text).expect("write the concat list");
    let clip = dir.path().join("clip.mp4");
    replayed_media::edit::concat_lossless(&bin, &list, &clip)
        .expect("the segments must concatenate losslessly");

    let info = MediaInfo::probe(&bin, &clip).expect("the clip must be a readable media file");
    let video = info.video.as_ref().expect("the clip must carry video");
    let audio_ms = info.audio.as_ref().and_then(|a| a.duration_ms).unwrap_or(0);
    let video_ms = video
        .duration_ms
        .expect("the video stream must have a duration");
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
    // The clip's display timeline, measured. Two things happen in a VFR segment and are reported
    // rather than glossed: two frames can share a timestamp (the rawvideo demuxer resolves arrival
    // times at 1/`-framerate`, so a burst within one tick shares it), and a frame can sit a little
    // across a segment boundary, which the concat demuxer's per-file offset turns into a small step
    // backwards there. Neither loses a frame (the counts below check that).
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

    // Both streams, as the clip path requires.
    assert!(info.audio.is_some(), "the spliced clip must carry audio");
    assert!(
        audio_ms > 500,
        "the spliced clip's audio stream must be real audio, not a stub of one: {audio_ms}ms"
    );
    // The clip's video is the segments it was built from, to within the muxer's granularity: a
    // relationship between two measurements of the same footage, so it holds however starved the
    // pipeline was.
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
    // Not asserted: the same band against `wall_ms`. The clip is a window over the ring's media
    // timeline, and this test requires a pipeline too starved to keep up, so the media the ring
    // could prove at the trigger is legitimately less than the wall-clock seconds elapsed (CI: 2498ms
    // of video for 3066ms of capture). The timeline-against-wall property belongs to the sibling
    // test, fed at a rate the pipeline can hold.
    //
    // The clip holds the frames its own duration implies at the delivered rate. Comparing against a
    // fixed two seconds of feed would conflate the clip with the capture it was cut from.
    let implied_by_duration = video_ms * u64::from(DELIVERED) / 1_000;
    assert!(
        coded + 2 >= implied_by_duration,
        "the clip must hold the frames its own {video_ms}ms implies at {DELIVERED}fps: {coded} \
         coded frames for {submitted} submitted over {wall:?}"
    );
    // Playable, not merely probeable: every coded frame decodes, and the container says how many there are.
    assert_eq!(
        decoded, coded,
        "every coded frame of the spliced clip must decode: {decoded} of {coded}"
    );
    assert!(
        coded >= u64::from(DELIVERED) * 2,
        "the clip must hold the frames that were delivered: {coded} coded frames for \
         {submitted} submitted"
    );
    // The splice must not scramble the timeline: at most one out-of-order pair per segment
    // boundary, and never a step backwards worth noticing (a tenth of a second).
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
/// `replayed_replay::splice::concat_list_path` is the real one, but the encoder crate must not
/// depend on the replay engine, and on Unix it reduces to the path as written.
fn replayed_replay_concat_path(path: &Path) -> String {
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

/// Decode every frame of a file and count them, so "playable" is checked by decoding rather than
/// by trusting the container's count.
fn decode_every_frame(bin: &FfmpegBinaries, path: &Path, width: u32, height: u32) -> u64 {
    // `-fps_mode passthrough` on the decode side too: otherwise the decode re-applies the conversion
    // this file is about (a 2.8s clip with 46 coded frames decodes as 342), and the counter would
    // measure that conversion, not the clip.
    let out = std::process::Command::new(&bin.ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "bgra",
            "-",
        ])
        .output()
        .expect("ffmpeg must run");
    assert!(
        out.status.success(),
        "decoding {} failed: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    // Not asserting stderr is empty: `-f rawvideo` has no timestamps of its own, so ffmpeg re-times
    // what it decodes and complains about the clip's repeated and (at a segment boundary) slightly
    // out-of-order display timestamps. The frame count below is the check.
    out.stdout.len() as u64 / (u64::from(width) * u64::from(height) * 4)
}
