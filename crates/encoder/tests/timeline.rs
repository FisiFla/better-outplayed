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
//! The fix has two halves and this test pins the second one: the capture loop is paced to
//! `encode.fps` (see `FramePacer` in the CLI), *and* the video input is timestamped from
//! arrival time (`-use_wallclock_as_timestamps 1`), so the media timeline is the wall clock
//! even when the delivered rate differs from the configured one. This test deliberately
//! delivers *half* the configured rate and asserts that the encoded footage still spans the
//! wall-clock seconds it was captured over — which is exactly what the old behaviour could
//! not do.
//!
//! Fed in real time through the same pacing the CLI uses, because that is the only way a
//! media timeline measured against a wall clock can be observed at all.

use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_encoder::{EncodeConfig, Encoder, FfmpegEncoder, VideoCodec};
use localplay_media::{FfmpegBinaries, MediaInfo};
use std::time::{Duration, Instant};

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

    // Drive the pipeline for FEED of *wall clock*, exactly as the CLI's loop does: both
    // stubs are real-time paced and `next_frame` waits at most 5ms for the next frame.
    let until = Instant::now() + FEED;
    let started = Instant::now();
    let mut frames: u64 = 0;
    while Instant::now() < until {
        if let Some(frame) = video.next_frame(Duration::from_millis(5)).expect("next frame") {
            encoder.submit_video(&frame).expect("submit video");
            frames += 1;
        }
        while let Some(block) = audio.next_buffer(Duration::ZERO).expect("next audio block") {
            encoder.submit_audio(&block).expect("submit audio");
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
         {captured:?}: {} segments, video {video_ms}ms, audio {audio_ms}ms; the frame count \
         alone would be {frame_count_ms}ms",
        segments.len()
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
    assert!(
        (2_800..=4_200).contains(&video_ms),
        "3s of capture must encode as ~3s of video, got {video_ms}ms"
    );
    // Segment *count* is a second view of the same thing: one segment per second of media,
    // so three seconds of wall clock cannot fit in the one or two files the old behaviour
    // produced from 1.5s of media.
    assert!(
        segments.len() >= 3,
        "3s of wall clock at 1s segments must produce >= 3 segment files, got {}",
        segments.len()
    );
    // Audio keeps its own exact timeline (the 48kHz sample count), which the video stream
    // now agrees with rather than drifting away from.
    assert!(
        audio_ms >= 2_800,
        "the sample-count audio timeline must cover the feed too, got {audio_ms}ms"
    );
    assert!(
        video_ms.abs_diff(audio_ms) < 700,
        "the two timelines must agree to within the muxer's granularity: video {video_ms}ms, \
         audio {audio_ms}ms"
    );
}
