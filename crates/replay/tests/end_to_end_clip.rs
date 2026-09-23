//! The PoC's core loop, exercised end to end off-Windows with stub sources.
use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_encoder::{EncodeConfig, Encoder, FfmpegEncoder, VideoCodec};
use localplay_media::{FfmpegBinaries, MediaInfo};
use localplay_replay::buffer::{BufferConfig, RingBuffer};
use std::time::Duration;

/// Initialise a subscriber so the clip's A/V drift line (spec §11 criterion 8,
/// emitted by `ClipSplicer::splice` at `info`) is observable. `try_init` keeps this
/// idempotent; with no `RUST_LOG` the default `warn` keeps ordinary runs quiet.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .try_init();
}

#[test]
fn triggering_produces_a_clip_with_video_and_audio_and_stays_under_the_cap() {
    init_tracing();
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let scratch = tempfile::tempdir().unwrap();
    let clips = tempfile::tempdir().unwrap();

    let encode = EncodeConfig::for_tests_software(
        VideoCodec::H264,
        64,
        48,
        10,
        scratch.path().to_path_buf(),
        1_000, // 1s segments (EncodeConfig.segment_ms is milliseconds)
    );
    let cfg = BufferConfig {
        pre_ms: 3_000,
        post_ms: 1_000,
        scratch_cap_bytes: 1_000_000,
        segment_ms: 1_000,
        clips_dir: clips.path().to_path_buf(),
    };

    let mut video = StubCapture::new(StubConfig { width: 64, height: 48, fps: 10 });
    let mut audio = StubAudio::new(AudioFormat::default());
    let mut encoder = FfmpegEncoder::spawn(&bin, &encode).unwrap();
    let mut ring = RingBuffer::start(
        &bin,
        cfg.clone(),
        scratch.path().to_path_buf(),
        "libx264".to_string(),
    )
    .expect("start ring buffer");

    // 6 seconds of timeline: segments 0..5 exist, so 0..4 are complete.
    for frame in video.drain_for(Duration::from_secs(6)) {
        encoder.submit_video(&frame).unwrap();
    }
    for block in audio.drain_for(Duration::from_secs(6)) {
        encoder.submit_audio(&block).unwrap();
    }
    encoder.finish().unwrap();

    ring.scan_once().expect("scan scratch dir");
    assert!(ring.stats().segments >= 4, "expected completed segments, got {}", ring.stats().segments);
    assert!(
        ring.stats().bytes_on_disk <= cfg.scratch_cap_bytes,
        "scratch exceeded its cap"
    );

    let clip = ring.trigger(4_000, "test-clip").expect("trigger");
    let info = MediaInfo::probe(&bin, &clip.path).unwrap();

    assert!(info.video.is_some(), "clip must have video");
    assert!(info.audio.is_some(), "clip must have audio");
    // pre=3s post=1s => 4s of timeline, segment-aligned so >= 3s.
    assert!(
        (3_000..=4_500).contains(&info.duration_ms),
        "clip duration {}ms unexpected",
        info.duration_ms
    );
    // Criterion 8: the clip's A/V offset must be computable from the probe (both
    // streams report a per-stream duration). The drift line itself is logged by
    // `ClipSplicer::splice`; this asserts the number behind it is sane. The tiny
    // 10 fps stub clip reliably lands around 200 ms, while the real 60 fps pipeline
    // measures tens of ms (verified against crate-produced segments); both are well
    // under this bound.
    let drift = info
        .av_drift()
        .expect("both streams must report a duration so A/V drift is computable");
    assert!(
        drift.delta_ms.abs() < 500,
        "stub clip A/V drift {}ms looks wrong (video {}ms, audio {}ms)",
        drift.delta_ms,
        drift.video_ms,
        drift.audio_ms
    );
    assert_eq!(clip.encoder, "libx264");
}
