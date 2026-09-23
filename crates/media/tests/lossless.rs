//! Proves the spec's principle 5: export never re-encodes.
use localplay_media::{edit, probe::MediaInfo, FfmpegBinaries};
use std::process::Command;

fn ffmpeg() -> FfmpegBinaries {
    FfmpegBinaries::discover(None).expect("ffmpeg on PATH (brew install ffmpeg)")
}

/// 3s of testsrc2 video + 440Hz sine audio, so there is a real audio stream to preserve.
fn fixture(dir: &std::path::Path) -> std::path::PathBuf {
    let out = dir.join("fixture.mp4");
    let bin = ffmpeg();
    let status = Command::new(&bin.ffmpeg)
        .args([
            "-v", "error", "-y",
            "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30",
            "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
            "-t", "3",
            "-c:v", "libx264", "-preset", "ultrafast", "-g", "30",
            // `-ac 2` matters: `sine` defaults to mono, but the real pipeline
            // captures 48kHz stereo, so a mono fixture would not exercise the
            // same muxing path (criterion 8).
            "-c:a", "aac", "-ac", "2", "-shortest",
        ])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "fixture generation failed");
    out
}

#[test]
fn remux_preserves_codec_and_produces_playable_output() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let src = fixture(dir.path());
    let dst = dir.path().join("remuxed.mp4");

    edit::remux_lossless(&bin, &src, &dst).expect("remux");

    let before = MediaInfo::probe(&bin, &src).unwrap();
    let after = MediaInfo::probe(&bin, &dst).unwrap();
    assert_eq!(before.video.as_ref().unwrap().codec, after.video.as_ref().unwrap().codec);
    assert_eq!(before.audio.as_ref().unwrap().codec, after.audio.as_ref().unwrap().codec);
    let drift = before.duration_ms.abs_diff(after.duration_ms);
    assert!(drift < 100, "duration drifted {drift}ms");
}

#[test]
fn trim_keeps_both_streams_and_shortens() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let src = fixture(dir.path());
    let dst = dir.path().join("trimmed.mp4");

    edit::trim_lossless(&bin, &src, &dst, 1000, 2000).expect("trim");

    let after = MediaInfo::probe(&bin, &dst).unwrap();
    assert!(after.video.is_some(), "video stream must survive a trim");
    assert!(after.audio.is_some(), "audio stream must survive a trim");
    // Keyframe snapping means this is approximate, not exact.
    assert!(
        (900..=2100).contains(&after.duration_ms),
        "trimmed duration {}ms outside tolerance",
        after.duration_ms
    );
}
