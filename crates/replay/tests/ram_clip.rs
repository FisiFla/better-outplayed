//! A clip cut out of a **RAM ring**: no scratch directory, and one file written.
//!
//! The whole point of the in-memory buffer, end to end. This test is the counterpart of
//! `end_to_end_clip.rs` (which drives the file-based ring through a real capture): here the
//! footage never touches the disk until the clip is saved, and the assertions are about that
//! happening — not about the pipeline in general, which the other test covers.
//!
//! Driven by `StubCapture`/`StubAudio` into a real ffmpeg in `FragmentedStream` mode, so the
//! bytes this ring ingests are the same shape a live capture produces: one `moof`+`mdat` per
//! forced keyframe, carrying both streams.

use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_encoder::{EncodeConfig, EncodeOutput, Encoder, FfmpegEncoder, VideoCodec};
use localplay_media::{edit::audio_titles, FfmpegBinaries, MediaInfo};
use localplay_replay::splice::ClipSplicer;
use localplay_replay::MemoryRingBuffer;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Record `seconds` of stub capture into a ring, and hold the stream's bytes.
///
/// Returns the ring and the directory that must stay empty: the two things this test is about.
fn ring_of_stub_capture(seconds: u64) -> (MemoryRingBuffer, tempfile::TempDir) {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut cfg = EncodeConfig::for_tests_software(
        VideoCodec::H264,
        64,
        48,
        30,
        dir.path().to_path_buf(),
        1_000,
    );
    cfg.output = EncodeOutput::FragmentedStream;

    let mut enc = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn the encoder");
    let stream = enc.take_output_stream().expect("the stream mode exposes its pipe");

    // Read on another thread: ffmpeg blocks when its stdout fills, which stops it draining
    // stdin and would hang this test rather than fail it.
    let done = Arc::new(AtomicBool::new(false));
    let finished = Arc::clone(&done);
    let reader = std::thread::spawn(move || {
        let mut stream = stream;
        let mut bytes = Vec::new();
        while !finished.load(Ordering::Relaxed) {
            let mut chunk = [0u8; 32 * 1024];
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
        }
        bytes
    });

    let mut video = StubCapture::new(StubConfig { width: 64, height: 48, fps: 30 });
    let mut audio = StubAudio::new(AudioFormat::default());
    video.start().expect("start the capture stub");
    audio.start().expect("start the audio stub");

    let until = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < until {
        if let Some(frame) = video.next_frame(Duration::from_millis(5)).expect("next frame") {
            enc.submit_video(frame).expect("submit video");
        }
        while let Some(block) = audio.next_buffer(Duration::ZERO).expect("next audio block") {
            enc.submit_audio(block).expect("submit audio");
        }
    }
    enc.finish().expect("flush the encoder");
    done.store(true, Ordering::Relaxed);
    let bytes = reader.join().expect("the reader thread");

    let mut ring = MemoryRingBuffer::new(256 * 1024 * 1024, 120_000);
    ring.push(&bytes).expect("the ring ingests the stream");
    (ring, dir)
}

#[test]
fn a_clip_saved_from_ram_is_playable_and_leaves_the_scratch_directory_empty() {
    let (ring, dir) = ring_of_stub_capture(3);
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");

    assert!(
        ring.header().is_some(),
        "the ring must have the header, or no clip can be assembled"
    );
    assert!(
        ring.span_ms() >= 1_000,
        "3s of capture must leave provable footage in the ring, got {}ms",
        ring.span_ms()
    );

    // Everything the ring holds, which is the largest clip it can serve.
    let window = ring
        .window(0, ring.span_ms() + 1_000)
        .expect("the ring can serve this range");
    assert!(
        window.segments.len() >= 2,
        "a clip of a few seconds must be a few fragments, got {}",
        window.segments.len()
    );

    // The clip goes where a clip belongs — and NOT into the scratch directory, which is the
    // directory that must stay empty.
    let out_dir = tempfile::tempdir().expect("a temp clips dir");
    let out = out_dir.path().join("ram-clip.mp4");
    let meta = ClipSplicer::splice_from_memory(&bin, &window, &out, "libx264")
        .expect("splicing from memory must produce a clip");

    assert!(out.is_file(), "the clip is on disk: {}", out.display());
    assert!(meta.size_bytes > 0, "and has bytes");
    assert_eq!(
        meta.size_bytes,
        std::fs::metadata(&out).expect("stat the clip").len(),
        "the reported size is the file's own"
    );
    assert!(
        meta.duration_ms >= 1_000,
        "a clip of several fragments must be seconds long, got {}ms",
        meta.duration_ms
    );

    // Both streams, by ffprobe, and the codec the encoder produced — nothing was decoded to
    // build this file.
    let info = MediaInfo::probe(&bin, &out).expect("probing the clip");
    let video = info.video.as_ref().expect("the clip must carry video");
    let audio = info.audio.as_ref().expect("and audio");
    assert_eq!(video.codec, "h264", "a lossless copy keeps the codec it was given");
    assert_eq!(audio.codec, "aac", "for both streams");

    // THE constraint: recording into a RAM ring writes nothing to the scratch directory, and
    // saving writes exactly one file — the clip.
    let scratch: Vec<String> = std::fs::read_dir(dir.path())
        .expect("reading the scratch directory")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        scratch.is_empty(),
        "buffering into RAM must not touch the scratch directory, and it holds {scratch:?}"
    );
    let clips: Vec<String> = std::fs::read_dir(out_dir.path())
        .expect("reading the clips directory")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(clips.len(), 1, "saving writes the clip and nothing else: {clips:?}");
}

#[test]
fn the_saved_clip_keeps_the_audio_track_names_the_encoder_wrote() {
    // The RAM path must not reintroduce the anonymous-tracks defect: a `-c copy` to a file
    // carries no per-stream metadata, so the save path has to re-apply the names. Here the
    // stream has ONE audio track (no microphone in these stubs), so it must come out named
    // "Game Audio" rather than nothing at all.
    let (ring, _dir) = ring_of_stub_capture(3);
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let window = ring.window(0, ring.span_ms() + 1_000).expect("a window");

    assert_eq!(
        window.audio_tracks(),
        1,
        "this fixture records no microphone, so the header declares one audio track"
    );
    assert_eq!(
        audio_titles(window.audio_tracks()),
        vec!["Game Audio"],
        "and the save path names that one track"
    );

    let out_dir = tempfile::tempdir().expect("a temp clips dir");
    let out = out_dir.path().join("named.mp4");
    ClipSplicer::splice_from_memory(&bin, &window, &out, "libx264").expect("the splice");

    let names = audio_stream_names(&bin, &out);
    assert_eq!(
        names,
        vec![Some("Game Audio".to_string())],
        "the clip's audio track must be named, not anonymous"
    );
}

/// The `name` tag of each audio stream, in order — what a player shows as the track's title.
fn audio_stream_names(bin: &FfmpegBinaries, path: &std::path::Path) -> Vec<Option<String>> {
    let out = std::process::Command::new(&bin.ffprobe)
        .args(["-v", "error", "-print_format", "json", "-show_streams"])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("ffprobe json");
    json["streams"]
        .as_array()
        .expect("a streams array")
        .iter()
        .filter(|s| s["codec_type"] == "audio")
        .map(|s| s["tags"]["name"].as_str().map(str::to_string))
        .collect()
}

/// The recorder's shape, without the recorder: the stream is **ingested on the thread that owns
/// the pipe**, concurrently with the frames being fed.
///
/// `ring_of_stub_capture` above collects the stream first and parses it afterwards. That is
/// easier to write and it is not what the recorder does — and the difference is exactly where a
/// stall hides. A reader that falls behind backs the whole pipeline up: ffmpeg blocks writing to
/// a full stdout, so it stops draining stdin, so the encoder's frame channel fills and the
/// submit side slows to whatever the reader is still moving. The symptom is not an error, it is
/// a recording that quietly runs at a fraction of its configured rate.
///
/// So this asserts the two halves that would show it: that the capture side kept its pace, and
/// that the ring holds the footage wall time says it should.
#[test]
fn a_live_stream_is_ingested_as_it_arrives_without_stalling_the_pipeline() {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut cfg = EncodeConfig::for_tests_software(
        VideoCodec::H264,
        64,
        48,
        30,
        dir.path().to_path_buf(),
        1_000,
    );
    cfg.output = EncodeOutput::FragmentedStream;

    let mut enc = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn the encoder");
    let stream = enc.take_output_stream().expect("the stream mode exposes its pipe");

    let ring = Arc::new(Mutex::new(MemoryRingBuffer::new(256 * 1024 * 1024, 120_000)));
    let reader = {
        let ring = Arc::clone(&ring);
        std::thread::spawn(move || {
            let mut stream = stream;
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break, // the encoder closed the pipe
                    Ok(n) => {
                        let Ok(mut ring) = ring.lock() else { break };
                        if ring.push(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
    };

    let mut video = StubCapture::new(StubConfig { width: 64, height: 48, fps: 30 });
    let mut audio = StubAudio::new(AudioFormat::default());
    video.start().expect("start the capture stub");
    audio.start().expect("start the audio stub");

    let until = Instant::now() + Duration::from_secs(4);
    let mut frames = 0u32;
    while Instant::now() < until {
        if let Some(frame) = video.next_frame(Duration::from_millis(5)).expect("next frame") {
            enc.submit_video(frame).expect("submit video");
            frames += 1;
        }
        while let Some(block) = audio.next_buffer(Duration::ZERO).expect("next audio block") {
            enc.submit_audio(block).expect("submit audio");
        }
    }
    enc.finish().expect("flush the encoder");
    reader.join().expect("the reader thread");

    // 4s at 30fps is ~120 frames. Half of that is a wide margin that still fails loudly if the
    // submit side is being throttled by a reader that cannot keep up.
    assert!(
        frames > 60,
        "the capture side stalled: {frames} frames in 4s at 30fps, which is not the stub's \
         doing — a reader that falls behind throttles the encoder"
    );

    let ring = ring.lock().expect("the ring");
    let span = ring.span_ms();
    assert!(
        span >= 3_000,
        "the ring holds {span}ms of provable footage after 4s of capture: the reader fell \
         behind and the pipeline paid for it"
    );
}
