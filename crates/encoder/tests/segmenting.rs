//! Drives the encoder with stub sources and asserts it produces segments
//! containing BOTH a video and an audio stream.
use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_encoder::{EncodeConfig, FfmpegEncoder, Encoder, VideoCodec};
use localplay_media::FfmpegBinaries;
use std::time::Duration;

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
        // MILLISECONDS. See the note in Task 10 — this unit is easy to get wrong
        // and a ">= 2 segments" assertion will not catch it.
        1_000,
    );

    let mut enc = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn encoder");
    let mut video = StubCapture::new(StubConfig { width: 64, height: 48, fps: 30 });
    let mut audio = StubAudio::new(AudioFormat::default());
    video.start().unwrap();
    audio.start().unwrap();

    // 3 seconds of timeline => at least 2 completed segments.
    for frame in video.drain_for(Duration::from_secs(3)) {
        enc.submit_video(&frame).expect("submit video");
    }
    for block in audio.drain_for(Duration::from_secs(3)) {
        enc.submit_audio(&block).expect("submit audio");
    }
    enc.finish().expect("flush encoder");

    let mut segments: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "mp4"))
        .collect();
    segments.sort();
    assert!(segments.len() >= 2, "expected >=2 segments, got {}", segments.len());

    let info = localplay_media::MediaInfo::probe(&bin, &segments[0]).unwrap();
    assert!(info.video.is_some(), "segment must contain video");
    assert!(info.audio.is_some(), "segment must contain audio");
    // Guards the `segment_ms` unit. Passing 1 instead of 1_000 silently produces
    // 1ms segments, which still satisfies the ">= 2 segments" check above — so
    // without this assertion the unit error goes unnoticed.
    assert!(
        (900..=1300).contains(&info.duration_ms),
        "segment duration {}ms should be ~1000ms (segment_ms is milliseconds)",
        info.duration_ms
    );
}
