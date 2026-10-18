//! The fragmented-stream output mode: no files on disk, and a stream a buffer can hold.
//!
//! This is the mode the in-memory replay buffer is built on. These tests prove what
//! `segmenting.rs` cannot:
//!
//! 1. Nothing is written to disk while recording (an idle replay buffer must not churn the SSD),
//!    asserted directly rather than inferred from "no segment files were found".
//! 2. The pipe splits into fragments with monotonic media times, carrying video and audio.
//! 3. Fragments plus the header form a real clip: a `-c copy` remux that ffprobe reads as
//!    video + audio of the right length, from bytes that only ever existed in memory.
//!
//! Fed in real time, as in `segmenting.rs`: the media timeline is the wall clock, so a burst of
//! stubbed frames is a few milliseconds of media.

use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_encoder::{EncodeConfig, EncodeOutput, Encoder, FfmpegEncoder, VideoCodec};
use localplay_media::{edit, FfmpegBinaries, FragmentSplitter, MediaInfo};
use std::io::Read;
use std::time::{Duration, Instant};

/// Drive the encoder for `seconds` of wall clock, reading its output stream on another thread.
///
/// The reader is required: a full stdout pipe blocks ffmpeg, which stops draining stdin, which
/// stalls `submit_video` and hangs the test instead of failing it.
fn record_stream(
    seconds: u64,
    cfg: &EncodeConfig,
    bin: &FfmpegBinaries,
) -> (Vec<u8>, std::path::PathBuf) {
    let mut enc = FfmpegEncoder::spawn(bin, cfg).expect("spawn the encoder");
    let stream = enc
        .take_output_stream()
        .expect("a fragmented-stream encoder must expose its pipe");
    assert!(
        enc.take_output_stream().is_none(),
        "the stream can only be taken once — there is one reader, and a second caller must \
         be told so rather than handed a half-stream"
    );

    let reader = std::thread::spawn(move || {
        let mut stream = stream;
        let mut out = Vec::new();
        stream.read_to_end(&mut out).map(|_| out)
    });

    let mut video = StubCapture::new(StubConfig {
        width: 64,
        height: 48,
        fps: 30,
    });
    let mut audio = StubAudio::new(AudioFormat::default());
    video.start().expect("start the capture stub");
    audio.start().expect("start the audio stub");

    let until = Instant::now() + Duration::from_secs(seconds);
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
        "30fps for {seconds}s must deliver ~{} frames, got {frames}",
        30 * seconds
    );
    enc.finish().expect("flush the encoder");

    let bytes = reader
        .join()
        .expect("the reader thread")
        .expect("reading the stream");
    (bytes, cfg.scratch_dir.clone())
}

fn stream_cfg(dir: &std::path::Path) -> EncodeConfig {
    let mut cfg = EncodeConfig::for_tests_software(
        VideoCodec::H264,
        64,
        48,
        30,
        dir.to_path_buf(),
        // Milliseconds; also the forced-keyframe interval, which makes a fragment boundary a cut point.
        1_000,
    );
    cfg.output = EncodeOutput::FragmentedStream;
    cfg
}

#[test]
fn the_stream_mode_writes_nothing_to_disk() {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().expect("a temp dir");
    let cfg = stream_cfg(dir.path());

    let (bytes, scratch) = record_stream(3, &cfg, &bin);
    assert!(
        !bytes.is_empty(),
        "the stream must carry the encoded footage"
    );

    let files: Vec<String> = std::fs::read_dir(&scratch)
        .expect("reading the scratch directory")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        files.is_empty(),
        "the whole point of the stream mode: NOTHING is written while recording, and the \
         scratch directory holds {files:?}"
    );
}

#[test]
fn the_stream_splits_into_fragments_with_monotonic_media_times_and_both_streams() {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().expect("a temp dir");
    let cfg = stream_cfg(dir.path());

    let (bytes, _scratch) = record_stream(3, &cfg, &bin);
    let mut splitter = FragmentSplitter::new();
    let fragments = splitter.push(&bytes).expect("the stream must split");
    let header = splitter
        .header()
        .expect("the header arrived before the fragments")
        .to_vec();

    assert!(
        fragments.len() >= 2,
        "3s at 1s keyframes must be at least two fragments, got {}",
        fragments.len()
    );
    let starts: Vec<u64> = fragments.iter().map(|f| f.start_ms).collect();
    assert_eq!(starts[0], 0, "the first fragment starts at zero");
    assert!(
        // Non-decreasing: a loaded runner can yield a tiny extra fragment sharing its neighbour's
        // start; time must never run backwards and must advance overall.
        starts.windows(2).all(|w| w[0] <= w[1]) && starts.last() > starts.first(),
        "media time must advance monotonically: {starts:?}"
    );
    assert!(
        fragments.iter().all(|f| f.keyframe),
        "every fragment starts on a keyframe, or a clip cut could begin mid-GOP"
    );

    // Both streams must be in the header the encoder wrote; a header missing a track would show up
    // only as a clip with one stream.
    assert!(
        header.windows(4).any(|w| w == b"vide"),
        "the header must declare a video track"
    );
    assert!(
        header.windows(4).any(|w| w == b"soun"),
        "and a sound track: a stream with no audio is a recording nobody can use"
    );
}

/// The save path, from bytes that never touched disk until now.
#[test]
fn the_fragments_remux_into_a_playable_clip_with_both_streams() {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().expect("a temp dir");
    let cfg = stream_cfg(dir.path());

    let (bytes, _scratch) = record_stream(3, &cfg, &bin);
    let mut splitter = FragmentSplitter::new();
    let fragments = splitter.push(&bytes).expect("the stream must split");
    let header = splitter.header().expect("a header").to_vec();

    // A clip is the header once plus a contiguous range of fragments. Dropping the first fragment
    // makes the range start mid-stream, as a trigger does; starting at zero would hide a timestamp bug.
    let mut assembled = header;
    for fragment in fragments.iter().skip(1) {
        assembled.extend_from_slice(&fragment.bytes);
    }
    let stream_path = dir.path().join("assembled.mp4");
    std::fs::write(&stream_path, &assembled).expect("writing the assembled stream");

    // The clip path: `-c copy`, no re-encoding (`localplay_media::edit`'s remux).
    let clip_path = dir.path().join("clip.mp4");
    edit::remux_lossless(&bin, &stream_path, &clip_path).expect("the clip must remux");

    let info = MediaInfo::probe(&bin, &clip_path).expect("probing the clip");
    assert!(info.video.is_some(), "the clip must carry video");
    assert!(info.audio.is_some(), "and audio");
    assert!(
        info.duration_ms >= 1_000,
        "a clip of two 1s fragments must be at least a second long, got {}ms",
        info.duration_ms
    );
}
