//! How many frames per second this machine can actually encode, measured before it records.
//!
//! # What this is, and what it is not
//!
//! A diagnostic and a pacing aid, not what makes the media timeline track real time. That is the
//! frame-rate conversion being off ([`video_output_args`]'s `-fps_mode passthrough`), which makes
//! the muxer keep each frame's arrival timestamp. A machine that cannot keep up drops frames
//! (a held frame in the picture) while the clock stays honest.
//!
//! What this number answers is how fast this machine encodes at the resolution and with the
//! encoder that will be used. Pacing capture to it (`localplay_recorder::FpsDecision`) stops the
//! pipeline paying for captures (on Windows a 33 MB GPU-to-CPU readback each) that the encoder
//! would throw away, and tells a user that `encode.fps = 30` is not reachable at 4K here.
//!
//! # Why it cannot be a timeline guarantee
//!
//! It measures the encode path only: raw frames into an encoder, out to the null muxer. The live
//! pipeline adds capture readback, copy, pipe and segment muxing, and the gap is large: at a 4K
//! output with a software encoder the probe reported 62fps sustainable and the pipeline achieved
//! 8 frames a second end to end. On Windows NVENC is cheap and the WGC readback is not, so the
//! probe is more optimistic there, not less.
//!
//! # What it measures, exactly
//!
//! The same ffmpeg invocation the recorder uses ([`video_input_args`] and [`video_output_args`],
//! shared with [`crate::FfmpegEncoder::spawn`]) writing to the null muxer. Frames go in over
//! `pipe:0` as raw BGRA, and only a frame ffmpeg accepted is counted: a completed write means the
//! encoder consumed that much of the stream.
//!
//! Two limitations:
//!
//! * It is a lower bound on a software encoder and accurate on a hardware one: a GPU encoder's
//!   throughput depends on resolution, not picture, while libx264's depends on content, so the
//!   probe frames carry a detailed moving pattern (see [`probe_frame`]).
//! * It does not measure the capture path (no readback, window composition or game), so the
//!   encoder's drop counter and rate-limited warning remain the report of what the pipeline achieved.
//!
//! # Bounds
//!
//! Frames are fed for [`PROBE_BUDGET`] and the child has [`PROBE_DRAIN_TIMEOUT`] to finish, past
//! which it is killed and reaped. Memory is one frame buffer at the capture's own size (33.2 MB
//! at 3840x2160).

use crate::ffmpeg::{video_input_args, video_output_args};
use crate::EncodeConfig;
use anyhow::{bail, Context, Result};
use localplay_media::{ffmpeg_reason, wait_with_deadline, FfmpegBinaries};
use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// How long frames are fed to the encoder before the rate is computed.
///
/// The entire startup cost of adapting, so a compromise:
///
/// * at 30fps this is ~45 frames, averaging out pipe fill and muxer jitter;
/// * long enough that a hardware encoder's session setup (low hundreds of milliseconds, excluded
///   by [`feed`]) cannot dominate;
/// * short enough to be a startup cost. `encode.adapt_fps = false` skips it and declares the
///   configured rate unmeasured.
pub const PROBE_BUDGET: Duration = Duration::from_millis(1500);

/// How long the encoder child gets to finish after the feeding stops.
///
/// A healthy child exits in well under a second once stdin closes; this is the ceiling for a
/// wedged one, which is killed and the probe fails rather than hanging startup.
pub const PROBE_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// A shortest window a rate may be computed over.
///
/// Dividing by (nearly) zero gives an absurd number that would be declared as the pipeline's
/// frame rate. A millisecond is far below any real window and far above nonsense arithmetic.
const MIN_RATE_WINDOW: Duration = Duration::from_millis(1);

/// What the probe measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThroughputMeasurement {
    /// Frames the encoder accepted per second, over [`ThroughputMeasurement::window`].
    pub fps: f64,
    /// Frames accepted within the measured window (the warm-up frame is not counted).
    pub frames: u64,
    /// How long that window lasted, measured from the first accepted frame.
    pub window: Duration,
    /// The size of the frames fed, the capture's own size. Encode cost is dominated by pixel count,
    /// so the probe runs at this size, not a small stand-in.
    pub source_size: (u32, u32),
    /// The size the encoder wrote. Differs from [`ThroughputMeasurement::source_size`] when
    /// `encode.output_size` scales, and the encoding cost follows this one.
    pub output_size: (u32, u32),
}

/// Measure the encode rate this machine can sustain at `cfg.source_size`.
///
/// `cfg` is the recording's configuration (codec, encoder, geometry, bitrate, keyframe schedule)
/// with `budget` replacing the live pipeline as the source of frames. `cfg.fps` is passed to
/// ffmpeg unchanged, and the caller decides what to do with the number.
///
/// Fails, with ffmpeg's own words where there are any, rather than guessing: an encoder that
/// cannot start, dies mid-probe, accepts no frames or refuses to exit is a failed measurement,
/// never "fast enough".
pub fn measure_sustainable_fps(
    bin: &FfmpegBinaries,
    cfg: &EncodeConfig,
    budget: Duration,
) -> Result<ThroughputMeasurement> {
    measure_with_drain(bin, cfg, budget, PROBE_DRAIN_TIMEOUT)
}

/// [`measure_sustainable_fps`] with the drain ceiling as a parameter, so the bound can be tested quickly.
fn measure_with_drain(
    bin: &FfmpegBinaries,
    cfg: &EncodeConfig,
    budget: Duration,
    drain: Duration,
) -> Result<ThroughputMeasurement> {
    let encoder = cfg.encoder_name();
    let describe = format!(
        "{encoder} at {}x{} ({}s budget)",
        cfg.source_size.0,
        cfg.source_size.1,
        budget.as_secs_f64()
    );

    let mut cmd = localplay_media::sidecar_command(&bin.ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(video_input_args(cfg))
        .args(video_output_args(cfg))
        // The null muxer: the encode is real, the container is not, so a probe leaves nothing on disk.
        .args(["-f", "null", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {} to measure {describe}", bin.ffmpeg.display()))?;

    let sink = child
        .stdin
        .take()
        .context("the probe child's stdin is unavailable")?;
    let frame = probe_frame(cfg.source_size);
    // One row of the frame in bytes: how far the pattern is rotated between frames.
    let stride = (cfg.source_size.0 as usize * 4).max(1);
    let frame_len = frame.len();
    let writer = std::thread::Builder::new()
        .name("encode-probe".into())
        .spawn(move || feed(sink, &frame, stride, budget))
        .context("spawning the probe's writer thread")?;

    // The whole call is bounded by the feeding window plus the drain ceiling. On expiry the child is
    // killed and reaped, which breaks the writer's blocked write so the join below cannot park.
    let waited = wait_with_deadline(&mut child, budget + drain);
    let fed = writer
        .join()
        .map_err(|_| anyhow::anyhow!("the probe's writer thread panicked"))?;
    let (frames, window) = fed;
    let output = child
        .wait_with_output()
        .context("collecting the probe child's output")?;

    if let Err(_timeout) = waited {
        bail!(
            "timed out measuring {describe}: the encoder did not finish {} frames in {:?} \
             plus a {:?} drain (exit status {}). This is a bound, not a measurement — raise \
             nothing, lower encode.output_size or encode.fps, or set encode.adapt_fps = false \
             to start without measuring.",
            frames + 1,
            budget,
            drain,
            output.status
        );
    }
    if !output.status.success() {
        bail!(
            "the encoder died while its rate was being measured ({describe}): {} — ffmpeg \
             said: {}. Set encode.adapt_fps = false to start without measuring (the pipeline \
             then declares encode.fps and drops what the encoder cannot take).",
            output.status,
            ffmpeg_reason(&String::from_utf8_lossy(&output.stderr))
        );
    }

    let Some(fps) = frames_per_second(frames, window) else {
        bail!(
            "the encoder accepted no frames to measure ({describe}, first frame {frame_len} \
             bytes): the probe fed frames for {:?} and counted none of them. ffmpeg's last \
             words: {}",
            window.max(budget),
            ffmpeg_reason(&String::from_utf8_lossy(&output.stderr))
        );
    };

    Ok(ThroughputMeasurement {
        fps,
        frames,
        window,
        source_size: cfg.source_size,
        output_size: cfg.output_size,
    })
}

/// Frames accepted per second, from a count and the window they were accepted over.
///
/// `None` when there is nothing honest to compute:
///
/// * `frames == 0`: nothing was accepted, a failed probe rather than a rate of zero;
/// * a window shorter than [`MIN_RATE_WINDOW`]: the quotient would be absurd and the caller
///   would declare it as a frame rate.
///
/// Divides by the window that actually elapsed, not the nominal budget, since an overrun would
/// inflate the rate that decides how fast the pipeline declares itself to be.
pub fn frames_per_second(frames: u64, window: Duration) -> Option<f64> {
    if frames == 0 || window < MIN_RATE_WINDOW {
        return None;
    }
    let fps = frames as f64 / window.as_secs_f64();
    fps.is_finite().then_some(fps)
}

/// Write `frame` to `sink` as fast as it is accepted, until `budget` elapses or the sink stops
/// taking it. Returns the frames accepted in the measured window and how long it was.
///
/// The first frame is written but not counted, and the window starts when its write completes:
/// that write does not finish until ffmpeg has opened the encoder and pulled the bytes, so
/// counting it would under-report and make the recording run slower than the hardware can.
///
/// The last frame may overrun the budget by its own write; dropping it from the count would bias
/// the rate down. The overrun is bounded by one frame's encode time and the caller's deadline
/// ([`PROBE_DRAIN_TIMEOUT`]).
///
/// Each frame is written from a rotating offset (no copy), so no two frames are byte-identical;
/// repeating one picture would report a rate no real content could reach (see [`probe_frame`]).
///
/// A sink that stops accepting frames (broken pipe) ends the loop, and dropping `sink` is the
/// child's EOF.
fn feed(mut sink: impl Write, frame: &[u8], stride: usize, budget: Duration) -> (u64, Duration) {
    let mut accepted: u64 = 0;
    let mut window = Duration::ZERO;
    let mut started: Option<Instant> = None;
    let mut offset = 0usize;

    loop {
        // One frame in two slices, so the rotation costs no memcpy. `split_at` yields
        // `(frame[..offset], frame[offset..])`, so the halves are written tail-first: ffmpeg receives
        // `frame[offset..] ++ frame[..offset]`, the pattern rotated by `offset`. The other order rebuilds
        // the frame byte for byte, making every frame the same picture (caught by
        // `the_feeder_writes_whole_frames_and_rotates_them`). Splitting a frame across two writes is
        // invisible to ffmpeg: the pipe is a byte stream.
        let (head, tail) = frame.split_at(offset);
        if sink.write_all(tail).is_err() || sink.write_all(head).is_err() {
            break;
        }
        offset = (offset + stride) % frame.len().max(1);

        match started {
            // The warm-up frame: written, deliberately not counted.
            None => started = Some(Instant::now()),
            Some(start) => {
                accepted += 1;
                window = start.elapsed();
                if window >= budget {
                    break;
                }
            }
        }
    }
    (accepted, window)
}

/// The frame the probe feeds: one BGRA frame at the capture's own source size.
///
/// Two choices, both about not measuring a cheaper encoder than the one that will record:
///
/// * the real capture size: encode cost is dominated by pixel count, so a small frame would
///   predict a rate several times too high for 4K. One 4K frame is 33.2 MB, the probe's whole
///   memory cost.
/// * a detailed, moving pattern: a flat frame is nearly free for a software encoder, and
///   identical consecutive frames cost almost nothing. The pattern is a deterministic
///   pseudo-random field that [`feed`] rotates between frames. A hardware encoder is unaffected
///   (its rate depends on resolution, not picture).
///
/// Deterministic, so two runs are comparable and a failure is reproducible.
fn probe_frame(size: (u32, u32)) -> Vec<u8> {
    let len = (size.0 as usize)
        .saturating_mul(size.1 as usize)
        .saturating_mul(4);
    let mut frame = vec![0u8; len];
    // xorshift64*: the point is a detailed pattern, not cryptography.
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut i = 0usize;
    while i < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bytes = state.to_le_bytes();
        let take = (len - i).min(bytes.len());
        frame[i..i + take].copy_from_slice(&bytes[..take]);
        i += bytes.len();
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VideoCodec;

    /// A configuration at a small size: these tests are about the arithmetic and bounds, not 4K speed.
    fn cfg(fps: u32) -> EncodeConfig {
        EncodeConfig::for_tests_software(VideoCodec::H264, 64, 48, fps, ".".into(), 1000)
    }

    fn ffmpeg() -> FfmpegBinaries {
        FfmpegBinaries::discover(None).expect("ffmpeg on PATH")
    }

    // ---- pure logic: frames over a duration -------------------------------------------

    #[test]
    fn a_rate_is_frames_over_the_window_that_elapsed() {
        assert_eq!(frames_per_second(30, Duration::from_secs(1)), Some(30.0));
        assert_eq!(
            frames_per_second(45, Duration::from_millis(1500)),
            Some(30.0)
        );
        // 100 frames in 4s is 25/s, not 100/1.
        assert_eq!(frames_per_second(100, Duration::from_secs(4)), Some(25.0));
    }

    /// The zero-duration and absurd cases: a rate computed by dividing by (nearly) nothing is not a
    /// measurement and would be declared as the frame rate.
    #[test]
    fn a_zero_or_absurd_duration_yields_no_rate_instead_of_a_huge_one() {
        assert_eq!(frames_per_second(30, Duration::ZERO), None);
        assert_eq!(frames_per_second(30, Duration::from_nanos(1)), None);
        assert_eq!(frames_per_second(30, Duration::from_micros(999)), None);
        assert_eq!(
            frames_per_second(0, Duration::from_secs(1)),
            None,
            "no frames is not 0fps"
        );
        // At the floor the arithmetic is ordinary again.
        assert_eq!(
            frames_per_second(3, Duration::from_millis(1)),
            Some(3_000.0)
        );
    }

    // ---- the probe against a real ffmpeg ----------------------------------------------

    /// The probe frame is deterministic and not uniform; a flat frame would let a software encoder
    /// report a rate real content cannot hold.
    #[test]
    fn the_probe_frame_is_deterministic_and_has_detail_in_it() {
        let frame = probe_frame((16, 4));
        assert_eq!(frame.len(), 16 * 4 * 4);
        assert_eq!(
            frame,
            probe_frame((16, 4)),
            "the same size always yields the same bytes"
        );
        let distinct: std::collections::HashSet<u8> = frame.iter().copied().collect();
        assert!(
            distinct.len() > 16,
            "a flat frame would be a degenerate case for a software encoder: {} distinct \
             byte values",
            distinct.len()
        );
    }

    /// Every frame written is a whole frame and no two consecutive frames are identical; the
    /// rotation keeps an encoder from encoding one picture repeatedly.
    #[test]
    fn the_feeder_writes_whole_frames_and_rotates_them() {
        let frame = probe_frame((16, 4));
        let stride = 16 * 4;
        let mut sink: Vec<u8> = Vec::new();
        let (accepted, window) = feed(&mut sink, &frame, stride, Duration::from_millis(2));
        assert!(
            accepted >= 1,
            "a sink that always accepts must yield a count: {accepted}"
        );
        assert!(
            window >= Duration::from_millis(2),
            "the window is the budget: {window:?}"
        );
        assert_eq!(
            sink.len() % frame.len(),
            0,
            "{} bytes written is not a whole number of {} byte frames",
            sink.len(),
            frame.len()
        );
        let frames = sink.len() / frame.len();
        assert!(
            frames as u64 > accepted,
            "the warm-up frame was written too: {frames}"
        );
        // Consecutive frames differ: the second one starts `stride` bytes into the pattern.
        let first = &sink[..frame.len()];
        let second = &sink[frame.len()..2 * frame.len()];
        assert_ne!(
            first,
            second,
            "every frame must be a different picture ({} byte frames, rotation stride {stride})",
            frame.len()
        );
        assert_eq!(&second[..frame.len() - stride], &frame[stride..]);
        assert_eq!(&second[frame.len() - stride..], &frame[..stride]);
    }

    /// The real encoder, through the real probe. Needs `test-encoders`, which makes libx264
    /// reachable off Windows.
    ///
    /// Asserts what the probe promises: a plausible positive rate, and a call that finishes inside
    /// its budget plus a small margin.
    #[cfg(feature = "test-encoders")]
    #[test]
    fn the_probe_measures_the_real_encoder_within_its_budget() {
        let cfg = cfg(30);
        let budget = Duration::from_millis(600);
        let started = Instant::now();
        let m = measure_sustainable_fps(&ffmpeg(), &cfg, budget).expect("libx264 can be timed");
        let elapsed = started.elapsed();

        eprintln!(
            "probe: {:.1} fps over {} frames in {:?} (budget {budget:?}), measured at {}x{}; \
             the call took {elapsed:?}",
            m.fps, m.frames, m.window, m.source_size.0, m.source_size.1
        );

        assert!(
            m.fps > 0.0,
            "a real encoder must yield a positive rate: {m:?}"
        );
        assert!(
            m.fps < 100_000.0,
            "64x48 libx264 cannot be anywhere near this fast, so the arithmetic is wrong: {m:?}"
        );
        assert_eq!(
            m.source_size,
            (64, 48),
            "the rate is about the size that was measured"
        );
        assert_eq!(m.output_size, (64, 48), "and where it was written");
        assert!(
            m.frames > 0,
            "the frames that were counted are reported: {m:?}"
        );
        // The window is the budget, within the one frame that may overrun it.
        assert!(
            m.window >= budget && m.window <= budget + Duration::from_millis(500),
            "the measured window must be the budget plus at most a frame: {:?}",
            m.window
        );
        assert!(
            elapsed <= budget + PROBE_DRAIN_TIMEOUT,
            "the whole probe is bounded by budget + drain: {elapsed:?}"
        );
    }

    /// A wedged child must be killed and reported rather than waited out. A leaked ffmpeg would sit
    /// on the user's machine for the session, so the writer thread must end too, which it does once
    /// the killed child closes the pipe's read end.
    #[cfg(unix)]
    #[test]
    fn a_wedged_encoder_is_killed_and_reported_within_its_deadline() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("a temp dir");
        let stub = dir.path().join("ffmpeg");
        // `exec` so the child killed is `sleep` itself; a forking shell would leave an orphan.
        std::fs::write(&stub, "#!/bin/sh\nexec sleep 120\n").expect("writing the stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("making the stub executable");
        let bin = FfmpegBinaries {
            ffmpeg: stub.clone(),
            ffprobe: stub,
        };

        let budget = Duration::from_millis(100);
        let drain = Duration::from_millis(300);
        let started = Instant::now();
        let err = measure_with_drain(&bin, &cfg(30), budget, drain)
            .expect_err("a child that never reads is not a measurement");
        let elapsed = started.elapsed();
        let msg = format!("{err:#}");
        assert!(msg.contains("timed out"), "the deadline is named: {msg}");
        assert!(
            elapsed < budget + drain + Duration::from_secs(2),
            "the probe waited {elapsed:?}, which is not bounded by {budget:?} + {drain:?}"
        );
    }

    /// An encoder that cannot start must produce an error naming what happened, never a number.
    #[test]
    fn a_dead_encoder_is_an_error_not_a_rate() {
        let mut cfg = cfg(30);
        // The hardware path's shape: a named encoder. An unknown name stands in for a vendor encoder
        // that cannot open its runtime (`h264_amf` / "DLL amfrt64.dll failed to open").
        cfg.software_encoder = Some("definitely_not_an_encoder");
        let err = measure_sustainable_fps(&ffmpeg(), &cfg, Duration::from_millis(200))
            .expect_err("a missing encoder cannot be measured");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("definitely_not_an_encoder"),
            "ffmpeg's words are kept: {msg}"
        );
        assert!(
            msg.contains("adapt_fps = false"),
            "the way out is named: {msg}"
        );
    }
}
