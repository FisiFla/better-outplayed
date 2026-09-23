//! Drives the encoder with stub sources and asserts it produces segments
//! containing BOTH a video and an audio stream.
//!
//! Fed in real time, because the encoder's media timeline is the wall clock (see the
//! module comment in `localplay_encoder::ffmpeg`): handing 3 seconds of stubbed frames over
//! in one burst puts every one of them within a few milliseconds of arrival and encodes,
//! correctly, as a few milliseconds of media. A live capture delivers frames over the
//! seconds they represent, and that is what this test now does — with the assertions
//! unchanged.
use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_encoder::{EncodeConfig, FfmpegEncoder, Encoder, VideoCodec};
use localplay_media::FfmpegBinaries;
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
        // MILLISECONDS. See the note in Task 10 — this unit is easy to get wrong
        // and a ">= 2 segments" assertion will not catch it.
        1_000,
    );

    let mut enc = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn encoder");
    let mut video = StubCapture::new(StubConfig { width: 64, height: 48, fps: 30 });
    let mut audio = StubAudio::new(AudioFormat::default());
    video.start().unwrap();
    audio.start().unwrap();

    // 3 seconds of *wall clock* of stub capture => at least 2 completed segments. Both
    // stubs are paced by real time and `next_frame` waits at most 5ms for the next frame,
    // so this loop drives them exactly as the CLI's capture loop does.
    let until = Instant::now() + Duration::from_secs(3);
    let mut frames = 0u64;
    while Instant::now() < until {
        if let Some(frame) = video.next_frame(Duration::from_millis(5)).expect("next frame") {
            enc.submit_video(frame).expect("submit video");
            frames += 1;
        }
        while let Some(block) = audio.next_buffer(Duration::ZERO).expect("next audio block") {
            enc.submit_audio(block).expect("submit audio");
        }
    }
    assert!(frames >= 60, "30fps for 3s must deliver ~90 frames, got {frames}");
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

/// The segment numbering can be continued, which is what stops a second run overwriting
/// the files the adopted ledger points at.
///
/// Deliberately fed in a burst: the assertion is about which *filenames* ffmpeg opens, and
/// that does not depend on how the timeline looks. The first output file is created as soon
/// as ffmpeg writes a packet into it.
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
    let mut video = StubCapture::new(StubConfig { width: 64, height: 48, fps: 30 });
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
