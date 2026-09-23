//! Regression test for the frame-size guard on the encoder's rawvideo pipe.
//!
//! The bug class this file exists to prevent: the encoder's video input is a flat byte
//! stream that ffmpeg slices into frames of the size it was spawned with
//! (`EncodeConfig::source_size`). Nothing on that path can notice a frame of a different
//! geometry — ffmpeg mis-reads every boundary after the first, the picture comes apart
//! in bands, and the segment files still look healthy while the CLI logs nothing. The
//! first real Windows run found exactly that shape: on a 4K display at 150% scaling the
//! capture item was 3840x2160 while the pipe had been declared 2560x1440, because the
//! size came from a DPI-virtualised `GetSystemMetrics`. Nothing failed; the picture was
//! simply wrong. The guard (`localplay_cli::pump_once_counted` → `guard_frame_size`)
//! turns that silence into an error naming both sizes, and this test drives it through
//! the real pump path over the real ffmpeg encoder.
//!
//! Gating: like `post_roll.rs`, this file is deliberately **not**
//! `#![cfg(feature = "test-encoders")]`. It needs `EncodeConfig::for_tests_software`,
//! and that exists because `localplay-encoder` is a dev-dependency of this package with
//! its `test-encoders` feature enabled (apps/localplay-cli/Cargo.toml) — true for every
//! test build, while the CLI's own `test-encoders` feature stays opt-in so the shipping
//! binary keeps no CPU-encoding fallback. A `cfg` gate on that feature would compile the
//! test away under the suite's command line (`--features
//! localplay-encoder/test-encoders`), which is the one thing it must not do.
//!
//! Needs ffmpeg on `PATH` with a working libx264, like the rest of the suite.

use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_cli::{pump_once, pump_once_counted, FramePacer};
use localplay_encoder::{EncodeConfig, Encoder, FfmpegEncoder, VideoCodec};
use localplay_media::FfmpegBinaries;

/// What the encoder's rawvideo pipe is declared as (`-s 32x24`).
const PIPE: (u32, u32) = (32, 24);
/// What the capture backend actually produces. Deliberately not `PIPE`: this is the
/// observed Windows shape, where the pipe was declared *smaller* than the frames.
const FRAME: (u32, u32) = (64, 48);
const FPS: u32 = 10;
const SEGMENT_MS: u64 = 1_000;

/// A stub source feeding a real ffmpeg encoder whose pipe is sized independently of it.
struct Fixture {
    /// Owns the encoder's scratch directory, so it outlives the encoder.
    _scratch: tempfile::TempDir,
    capture: StubCapture,
    audio: StubAudio,
    encoder: FfmpegEncoder,
}

impl Fixture {
    /// `pipe` is the size the rawvideo pipe is declared with; `frame` is the size the
    /// capture stub actually produces. They are separate arguments on purpose.
    fn new(pipe: (u32, u32), frame: (u32, u32)) -> Self {
        let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
        let scratch = tempfile::tempdir().expect("a scratch temp dir");
        let encode = EncodeConfig::for_tests_software(
            VideoCodec::H264,
            pipe.0,
            pipe.1,
            FPS,
            scratch.path().to_path_buf(),
            SEGMENT_MS,
        );
        let mut capture =
            StubCapture::new(StubConfig { width: frame.0, height: frame.1, fps: FPS });
        let mut audio = StubAudio::new(AudioFormat::default());
        capture.start().expect("start the capture stub");
        audio.start().expect("start the audio stub");
        let encoder = FfmpegEncoder::spawn(&bin, &encode).expect("spawn encoder");
        Self { _scratch: scratch, capture, audio, encoder }
    }
}

#[test]
fn a_frame_that_disagrees_with_the_declared_pipe_size_is_refused_by_name() {
    // The pipe is declared 32x24; the backend hands over 64x48 frames.
    let mut fx = Fixture::new(PIPE, FRAME);
    assert_eq!(
        fx.encoder.source_size(),
        PIPE,
        "sanity: the encoder's pipe is declared with the configured source size"
    );

    let mut pacer = FramePacer::new(FPS);
    let err = pump_once(&mut pacer, &mut fx.capture, &mut fx.audio, &mut fx.encoder).expect_err(
        "a 64x48 frame on a pipe declared 32x24 must fail, not be fed to ffmpeg",
    );
    let msg = format!("{err:#}");
    eprintln!("refused: {msg}");

    // Both sizes, by value, so a reader does not have to know which is which.
    assert!(
        msg.contains("64x48"),
        "the error must name the size of the frame that arrived: {msg}"
    );
    assert!(
        msg.contains("32x24"),
        "the error must name the configured source size of the pipe: {msg}"
    );
    // ...and what is actually at stake.
    assert!(msg.contains("pipe"), "the error must say which link is broken: {msg}");
    assert!(
        msg.contains("mis-read"),
        "the error must say what would have happened to the bytes: {msg}"
    );
}

#[test]
fn a_frame_that_matches_the_declared_pipe_size_is_still_submitted() {
    // The same machine, correctly wired: pipe and frames agree. The guard must be
    // invisible here — a guard that refuses everything is not a guard.
    let mut fx = Fixture::new(PIPE, PIPE);
    // The pacer admits the first frame immediately, so this is the frame's own guard that
    // is being exercised here — not the rate limiter.
    let mut pacer = FramePacer::new(FPS);
    let submitted = pump_once_counted(&mut pacer, &mut fx.capture, &mut fx.audio, &mut fx.encoder)
        .expect("a frame the pipe was declared for must be submitted");
    assert_eq!(submitted, 1, "the matching frame must reach the encoder");
    fx.encoder.finish().expect("flush encoder");
}
