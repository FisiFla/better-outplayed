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

// The test bodies use `unwrap()` throughout; production code does not.
#![allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks)]
use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_encoder::{EncodeConfig, EncodeOutput, Encoder, FfmpegEncoder, VideoCodec};
use localplay_media::{edit::audio_titles, FfmpegBinaries, MediaInfo};
use localplay_replay::splice::ClipSplicer;
use localplay_replay::MemoryRingBuffer;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Build a ring from `seconds` of synthetic footage, with the scratch directory the tests
/// assert stays empty.
///
/// The stream is made offline (ffmpeg from lavfi sources, `empty_moov+frag_keyframe+default_base_moof`,
/// a keyframe a second, H.264 + AAC), so the span and fragments do not depend on host speed.
/// The real-time capture -> encoder -> ring path keeps its own test below.
fn ring_of_stub_capture(seconds: u64) -> (MemoryRingBuffer, tempfile::TempDir) {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().expect("a temp dir");
    let out = std::process::Command::new(&bin.ffmpeg)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x48:rate=30",
        ])
        .args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo"])
        .args(["-t", &seconds.to_string()])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "30"])
        .args(["-force_key_frames", "expr:gte(t,n_forced*1)"])
        .args(["-c:a", "aac", "-f", "mp4"])
        .args([
            "-movflags",
            "empty_moov+frag_keyframe+default_base_moof",
            "pipe:1",
        ])
        .output()
        .expect("spawn ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut ring = MemoryRingBuffer::new(256 * 1024 * 1024, 120_000);
    ring.push(&out.stdout).expect("the ring ingests the stream");
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
    assert_eq!(
        video.codec, "h264",
        "a lossless copy keeps the codec it was given"
    );
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
    assert_eq!(
        clips.len(),
        1,
        "saving writes the clip and nothing else: {clips:?}"
    );
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
    let stream = enc
        .take_output_stream()
        .expect("the stream mode exposes its pipe");

    let ring = Arc::new(Mutex::new(MemoryRingBuffer::new(
        256 * 1024 * 1024,
        120_000,
    )));
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

    let mut video = StubCapture::new(StubConfig {
        width: 64,
        height: 48,
        fps: 30,
    });
    let mut audio = StubAudio::new(AudioFormat::default());
    video.start().expect("start the capture stub");
    audio.start().expect("start the audio stub");

    let until = Instant::now() + Duration::from_secs(4);
    let mut frames = 0u32;
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

/// The window contract a trigger depends on: **given footage covering `[T - pre, T + post)`,
/// the ring selects all of it.**
///
/// This is the arithmetic that came back wrong when the recorder was first wired to this ring,
/// and it is worth a test of its own because the failure is silent rather than loud. `span_ms`
/// is the newest fragment's *end*, so if a trigger resolved its window against `span_ms` at the
/// instant it fired, the post-roll half of the window would reach past the footage the ring
/// holds and the clip would come back short by exactly the post-roll — a 3000ms request
/// producing 2000ms, which is what was observed.
///
/// What stops that is the recorder's post-roll wait: it keeps pumping until the span has passed
/// `T + post` *before* splicing, so by the time the window is resolved the footage exists. The
/// `t` below is therefore `span - post`, which is where that wait leaves the timeline, and the
/// assertion is that the ring then covers the whole request. The recorder's half of the
/// invariant — that it really waits — is the recorder's own test's business.
///
/// The window is still built against the front the ring reports through `segments()` (the same
/// closed-fragment set `window` selects from) rather than an assumed `0`.
#[test]
fn a_window_inside_the_held_footage_covers_the_whole_request() {
    const PRE: u64 = 2_000;
    const POST: u64 = 1_000;
    // The footage is synthetic and exact (no capture timing), so 8s is simply comfortably more
    // than the PRE + POST = 3s the window needs.
    let (ring, _dir) = ring_of_stub_capture(8);

    let span = ring.span_ms();
    // The oldest footage the ring can still cut from — asked of the ring rather than assumed to
    // be `0`. `window` selects only *closed* fragments, so this is the front of that same set.
    let front = ring
        .segments()
        .find(|s| s.is_closed())
        .expect("the ring ingested at least one closed fragment")
        .start_ms;

    // The fixture must hold more than a whole window measured from the front the ring reports,
    // or no window can sit wholly inside the footage and the test is asking the ring a question
    // it cannot answer. Stated against the ring's own numbers, this is the same precondition the
    // old `t > PRE` was for a ring whose front happened to be zero.
    assert!(
        span - front >= PRE + POST,
        "the fixture holds {}ms of footage ({front}..{span}), less than one {}-ms window",
        span - front,
        PRE + POST
    );

    // Anchored at the ring's reported front, not at `span`: frame timestamps are arrival times,
    // so a loaded runner can leave a hole that makes a window measured back from `span` truncate.
    let start = front;
    let t = start + PRE;
    let window = ring
        .window(start, t + POST)
        .expect("the ring can serve a window inside its own footage");

    assert!(
        !window.truncated_front,
        "the pre-roll is inside the held footage, so nothing should be reported as truncated"
    );
    // One frame of slack: the fragments carry whole frames, and at 30fps a selection can end a
    // frame short of the request without omitting any footage the ring held.
    assert!(
        window.duration_ms() + 40 >= PRE + POST,
        "a {}ms window resolved to {}ms of footage",
        PRE + POST,
        window.duration_ms()
    );

    // And the negative half of the contract, which is what the post-roll wait exists to avoid:
    // a window reaching past the held footage really does come back short. Stating it here keeps
    // the reason for the wait visible to anyone who changes either side.
    let past_the_end = ring
        .window(t, t + 10_000)
        .expect("a window at the end of the footage still selects something");
    assert!(
        past_the_end.duration_ms() < 10_000,
        "a window past the held footage must report less than it asked for, not claim the rest"
    );
}
