//! Export never re-encodes: lossless edits preserve the streams they copy.
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
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "3",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "30",
            // `-ac 2` matters: `sine` defaults to mono, but the real pipeline captures 48kHz stereo, so a
            // mono fixture would not exercise the same muxing path.
            "-c:a",
            "aac",
            "-ac",
            "2",
            "-shortest",
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
    assert_eq!(
        before.video.as_ref().unwrap().codec,
        after.video.as_ref().unwrap().codec
    );
    assert_eq!(
        before.audio.as_ref().unwrap().codec,
        after.audio.as_ref().unwrap().codec
    );
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

/// 2s of video and two audio tracks, tagged as the encoder tags a recording with a microphone:
/// `-map 0:v -map 1:a -map 2:a` plus the two titles.
///
/// The titles come from [`edit::audio_titles`] rather than being rewritten, so this fixture
/// cannot drift from the pipeline it stands in for (a drifted literal is how the defect below
/// stayed invisible).
fn two_track_fixture(dir: &std::path::Path, tag: &str) -> std::path::PathBuf {
    let out = dir.join(format!("two-track-{tag}.mp4"));
    let bin = ffmpeg();
    let mut cmd = Command::new(&bin.ffmpeg);
    cmd.args([
        "-v",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x180:rate=30",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=200:sample_rate=48000",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=6000:sample_rate=48000",
        "-t",
        "2",
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-map",
        "2:a",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-g",
        "30",
        "-c:a",
        "aac",
        "-ac",
        "2",
    ]);
    for (index, title) in edit::audio_titles(2).iter().enumerate() {
        cmd.arg(format!("-metadata:s:a:{index}"))
            .arg(format!("title={title}"));
    }
    let status = cmd.arg(&out).status().expect("spawn ffmpeg");
    assert!(
        status.success(),
        "the two-track fixture could not be encoded"
    );
    out
}

/// The `name` tag of each audio stream, in order; `None` for an unnamed track. `name`, not
/// `title`: ffmpeg's MP4 muxer stores `-metadata:s:a:N title=...` in the track's `name` atom,
/// which ffprobe reports as `tags.name`.
fn audio_names(bin: &FfmpegBinaries, path: &std::path::Path) -> Vec<Option<String>> {
    let out = Command::new(&bin.ffprobe)
        .args(["-v", "error", "-print_format", "json", "-show_streams"])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("ffprobe json");
    json["streams"]
        .as_array()
        .expect("ffprobe must report a streams array")
        .iter()
        .filter(|s| s["codec_type"] == "audio")
        .map(|s| s["tags"]["name"].as_str().map(str::to_string))
        .collect()
}

/// The concatenation keeps the audio tracks named.
///
/// A `-c copy` through the concat demuxer does not carry per-stream metadata. The encoder's own
/// segments are tagged (`tags: { "name": "Game Audio" }`), but after `-map 0 -c copy` neither
/// name survives, with or without `-map_metadata 0`. Every clip and session file made from
/// segments therefore had anonymous audio tracks, with nothing saying which was the game and
/// which the microphone. This is the regression test for the concat re-applying the names.
#[test]
fn concat_keeps_the_audio_track_names_the_encoder_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let a = two_track_fixture(dir.path(), "a");
    let b = two_track_fixture(dir.path(), "b");

    let expected: Vec<Option<String>> = edit::audio_titles(2)
        .iter()
        .map(|t| Some((*t).to_string()))
        .collect();
    assert_eq!(
        audio_names(&bin, &a),
        expected,
        "the fixture itself must be tagged the way the encoder tags a recording"
    );

    let list = dir.path().join("concat.txt");
    let total = edit::write_concat_list(&[a, b], &list).expect("the concat list");
    let dst = dir.path().join("clip.mp4");
    edit::concat_lossless_sized(&bin, &list, &dst, total, 2).expect("the concat");

    assert_eq!(
        audio_names(&bin, &dst),
        expected,
        "the clip's audio tracks must be named, not two anonymous 'Audio' streams"
    );
    // The naming must not have cost a track: the counts above only prove the tags, and the
    // original defect was a missing stream before it was a missing name.
    assert_eq!(
        audio_names(&bin, &dst).len(),
        2,
        "both audio tracks are still there"
    );
    assert!(
        MediaInfo::probe(&bin, &dst).unwrap().video.is_some(),
        "and so is the video"
    );
}

/// Each chapter as `(start ms, end ms, title)`, read back with ffprobe as a player would.
fn chapters(bin: &FfmpegBinaries, path: &std::path::Path) -> Vec<(u64, u64, String)> {
    let out = Command::new(&bin.ffprobe)
        .args(["-v", "error", "-print_format", "json", "-show_chapters"])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("ffprobe json");
    let ms = |v: &serde_json::Value| {
        (v.as_str().unwrap().parse::<f64>().unwrap() * 1000.0).round() as u64
    };
    json["chapters"]
        .as_array()
        .expect("ffprobe must report a chapters array")
        .iter()
        .map(|c| {
            let title = c["tags"]["title"].as_str().unwrap().to_string();
            (ms(&c["start_time"]), ms(&c["end_time"]), title)
        })
        .collect()
}

#[test]
fn chapters_are_written_without_costing_a_stream_or_a_track_name() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let clip = two_track_fixture(dir.path(), "chapters");
    let marks = [
        edit::Chapter {
            start_ms: 500,
            title: "Kill".into(),
        },
        edit::Chapter {
            start_ms: 1200,
            title: "Round start".into(),
        },
    ];

    edit::add_chapters(&bin, &clip, &marks, 2000).expect("adding chapters");

    assert_eq!(
        chapters(&bin, &clip),
        vec![
            (0, 500, edit::LEAD_IN_TITLE.to_string()),
            (500, 1200, "Kill".to_string()),
            (1200, 2000, "Round start".to_string()),
        ]
    );
    let expected: Vec<Option<String>> = edit::audio_titles(2)
        .iter()
        .map(|t| Some((*t).to_string()))
        .collect();
    assert_eq!(
        audio_names(&bin, &clip),
        expected,
        "the remux must keep both named tracks"
    );
    assert!(MediaInfo::probe(&bin, &clip).unwrap().video.is_some());
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".chapters."))
        .collect();
    assert!(
        leftovers.is_empty(),
        "scratch files left behind: {leftovers:?}"
    );
}

#[test]
fn a_clip_with_no_chapter_inside_it_is_left_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let clip = fixture(dir.path());
    let before = std::fs::read(&clip).unwrap();

    edit::add_chapters(&bin, &clip, &[], 3000).expect("no chapters");
    let late = [edit::Chapter {
        start_ms: 3000,
        title: "Late".into(),
    }];
    edit::add_chapters(&bin, &clip, &late, 3000).expect("only a chapter past the end");

    assert_eq!(std::fs::read(&clip).unwrap(), before);
}

#[test]
fn a_failed_chapter_remux_leaves_the_clip_intact() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let clip = fixture(dir.path());
    let before = std::fs::read(&clip).unwrap();
    // A directory in the scratch output's place makes ffmpeg's write fail.
    std::fs::create_dir(clip.with_extension("chapters.mp4")).unwrap();

    let marks = [edit::Chapter {
        start_ms: 0,
        title: "Kill".into(),
    }];
    assert!(edit::add_chapters(&bin, &clip, &marks, 3000).is_err());
    assert_eq!(
        std::fs::read(&clip).unwrap(),
        before,
        "the original clip must survive"
    );
}
