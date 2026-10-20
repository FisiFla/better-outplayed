//! Drives the encoder with stub sources and asserts segments contain both a video and an audio stream.
//!
//! Fed in real time: the media timeline is the wall clock (see `replayed_encoder::ffmpeg`), so
//! a burst of stubbed frames encodes as a few milliseconds of media.

// An integration test is a crate of its own, so the crate root's `cfg_attr(test, ...)`
// exemption does not reach it: the same rule, stated here.
#![allow(clippy::unwrap_used)]

use replayed_capture::stub::{StubAudio, StubCapture, StubConfig};
use replayed_capture::{AudioBackend, AudioFormat, CaptureBackend};
use replayed_encoder::{EncodeConfig, Encoder, FfmpegEncoder, VideoCodec};
use replayed_media::FfmpegBinaries;
use std::time::{Duration, Instant};

#[test]
fn produces_segments_with_both_a_video_and_an_audio_stream() {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().unwrap();

    let cfg = EncodeConfig::for_tests_software(
        VideoCodec::H264,
        64,
        48,
        30,
        dir.path().to_path_buf(),
        // Milliseconds. A ">= 2 segments" assertion alone would not catch a unit mistake.
        1_000,
    );

    let mut enc = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn encoder");
    let mut video = StubCapture::new(StubConfig {
        width: 64,
        height: 48,
        fps: 30,
    });
    let mut audio = StubAudio::new(AudioFormat::default());
    video.start().unwrap();
    audio.start().unwrap();

    // 3 seconds of wall clock => at least 2 completed segments. Both stubs are real-time paced and
    // `next_frame` waits at most 5ms, so this drives them as the CLI's capture loop does.
    let until = Instant::now() + Duration::from_secs(3);
    let mut frames = 0u64;
    while Instant::now() < until {
        if let Some(frame) = video
            .next_frame(Duration::from_millis(5))
            .expect("next frame")
        {
            enc.submit_video(frame).expect("submit video");
            frames += 1;
        }
        while let Some(block) = audio.next_buffer(Duration::ZERO).expect("next audio block") {
            enc.submit_audio(block).expect("submit audio");
        }
    }
    assert!(
        frames >= 60,
        "30fps for 3s must deliver ~90 frames, got {frames}"
    );
    enc.finish().expect("flush encoder");

    let mut segments: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "mp4"))
        .collect();
    segments.sort();
    assert!(
        segments.len() >= 2,
        "expected >=2 segments, got {}",
        segments.len()
    );

    let info = replayed_media::MediaInfo::probe(&bin, &segments[0]).unwrap();
    assert!(info.video.is_some(), "segment must contain video");
    assert!(info.audio.is_some(), "segment must contain audio");
    // Guards the `segment_ms` unit: 1 instead of 1_000 yields 1ms segments, which still pass the
    // ">= 2 segments" check above.
    assert!(
        (900..=1300).contains(&info.duration_ms),
        "segment duration {}ms should be ~1000ms (segment_ms is milliseconds)",
        info.duration_ms
    );
}

/// Segment numbering can be continued, so a second run cannot overwrite the files the adopted
/// ledger points at.
///
/// Fed in a burst on purpose: the assertion is about which filenames ffmpeg opens, not the timeline.
#[test]
fn the_encoder_starts_segment_numbering_at_start_number() {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().unwrap();

    let mut cfg = EncodeConfig::for_tests_software(
        VideoCodec::H264,
        64,
        48,
        30,
        dir.path().to_path_buf(),
        1_000,
    );
    // What the CLI computes from the adopted ledger and the scratch directory.
    cfg.start_number = 7;

    let mut enc = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn encoder");
    let mut video = StubCapture::new(StubConfig {
        width: 64,
        height: 48,
        fps: 30,
    });
    let mut audio = StubAudio::new(AudioFormat::default());
    video.start().expect("start the capture stub");
    audio.start().expect("start the audio stub");
    for frame in video.drain_for(Duration::from_millis(200)) {
        enc.submit_video(frame).expect("submit video");
    }
    for block in audio.drain_for(Duration::from_millis(200)) {
        enc.submit_audio(block).expect("submit audio");
    }
    enc.finish().expect("flush encoder");

    let names: Vec<String> = std::fs::read_dir(dir.path())
        .expect("read the scratch dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".mp4"))
        .collect();
    assert_eq!(
        names,
        vec!["seg-000007.mp4".to_string()],
        "the muxer must continue numbering from `-segment_start_number`, not restart at 0"
    );
}
