//! How many frames per second this machine can actually encode, measured before it records.
//!
//! # Why this exists
//!
//! The pipeline used to *declare* the configured `encode.fps` in two places at once — the
//! encoder child's `-framerate` and the capture loop's pacer — and simply drop whatever the
//! encoder could not take. On real 4K hardware that is what happened: ~24fps sustained
//! against a configured 30, ~45% of delivered frames dropped, and a media timeline that ran
//! at a fraction of real time (issues #1 and #2). A pipeline that declares a rate it cannot
//! deliver is lying about its own timeline in the only place that matters: the encoder's
//! timestamps are what a clip's duration is made of.
//!
//! So the rate is measured first, at the resolution that will actually be captured, with the
//! encoder that will actually be used — and the number that comes out is the number the whole
//! pipeline then declares (see `localplay_recorder::FpsDecision`).
//!
//! # What it measures, exactly
//!
//! The same ffmpeg invocation the recorder uses — [`video_input_args`] and
//! [`video_output_args`], shared with [`crate::FfmpegEncoder::spawn`] so the two cannot
//! describe different encoders — writing to the null muxer instead of to scratch segments.
//! Frames go in over `pipe:0` as raw BGRA, exactly as the live capture feeds them, and the
//! *only* thing counted is a frame ffmpeg accepted: a write that completes means the pipe had
//! room for those bytes, which means the encoder has consumed that much of the stream.
//!
//! Two honest limitations, both stated rather than hidden:
//!
//! * **It is a lower bound on a software encoder and an accurate figure on a hardware one.**
//!   A GPU encoder's throughput is fixed-function — it depends on the resolution, not on the
//!   picture — while libx264's depends heavily on content. The probe's frames carry a
//!   detailed moving pattern precisely so that the software case is not flattered (see
//!   [`probe_frame`]), and the shipping path uses a hardware encoder (spec §3.2).
//! * **It measures the encode path, not the capture path.** The frames are synthetic: no
//!   GPU-to-CPU readback, no window composition, no game. A machine whose *capture* cost also
//!   matters can therefore still fall slightly short of the measured rate; that is what the
//!   encoder's drop counter and its rate-limited warning remain for.
//!
//! # Bounds
//!
//! Bounded in time: frames are fed for [`PROBE_BUDGET`], and the child has
//! [`PROBE_DRAIN_TIMEOUT`] to finish after that — past which it is killed and reaped, never
//! leaked. Bounded in memory: one frame buffer at the capture's own size, the same order as
//! the frames the pipeline already holds (33.2 MB at 3840x2160).

use crate::ffmpeg::{video_input_args, video_output_args};
use crate::EncodeConfig;
use anyhow::{bail, Context, Result};
use localplay_media::{ffmpeg_reason, wait_with_deadline, FfmpegBinaries};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long frames are fed to the encoder before the rate is computed.
///
/// This is the entire startup cost of adapting, so it is a compromise, and the compromise is
/// deliberate:
///
/// * at 30fps this is ~45 frames, which averages out the pipe's fill and the muxer's
///   short-term jitter rather than reporting one scheduling hiccup;
/// * 1.5s is long enough that a hardware encoder's session setup and first submissions
///   (measured in the low hundreds of milliseconds, and excluded outright — see
///   [`feed`]) cannot dominate the result;
/// * and it is short enough to be a startup cost rather than a stall. A user who does not
///   want to pay it sets `encode.adapt_fps = false` and gets the configured rate declared
///   unmeasured, exactly as before.
pub const PROBE_BUDGET: Duration = Duration::from_millis(1500);

/// How long the encoder child gets to finish after the feeding stops.
///
/// After its stdin closes, a healthy child encodes what is already in the pipe and exits in
/// well under a second. This is the ceiling for a wedged one, not a budget an honest one
/// needs: past it the child is killed and the probe fails with a clear error rather than
/// hanging startup or leaving a process behind.
pub const PROBE_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// A shortest window a rate may be computed over.
///
/// Division by a zero (or near-zero) duration yields infinity or an absurd number, and an
/// absurd number here would be *declared as the pipeline's frame rate*. A millisecond is far
/// below any real measurement window and far above the arithmetic that produces nonsense.
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
    /// The size of the frames that were fed — the capture's own size, i.e. the rawvideo
    /// pipe. Encode cost is dominated by the pixel count, which is why the probe runs at
    /// this size and not at a small stand-in.
    pub source_size: (u32, u32),
    /// The size the encoder wrote. Differs from [`ThroughputMeasurement::source_size`] when
    /// `encode.output_size` asks for scaling, and the *encoding* cost follows this one — so
    /// a report of what was measured has to name both.
    pub output_size: (u32, u32),
}

/// Measure the encode rate this machine can sustain at `cfg.source_size`.
///
/// `cfg` is the *recording's* configuration — same codec, same encoder, same geometry, same
/// bitrate and keyframe schedule — with `budget` replacing the live pipeline as the source of
/// frames. The declared rate in `cfg.fps` is passed through to ffmpeg unchanged (it is part of
/// the invocation being measured), and the caller decides what to do with the number.
///
/// Fails, with ffmpeg's own words where there are any, rather than guessing: an encoder that
/// cannot start, dies mid-probe, accepts no frames at all, or refuses to exit is a *failed
/// measurement*, and the caller must not treat a missing number as "fast enough".
pub fn measure_sustainable_fps(
    bin: &FfmpegBinaries,
    cfg: &EncodeConfig,
    budget: Duration,
) -> Result<ThroughputMeasurement> {
    measure_with_drain(bin, cfg, budget, PROBE_DRAIN_TIMEOUT)
}

/// [`measure_sustainable_fps`] with the drain ceiling as a parameter, so the bound can be
/// tested without waiting out the production one.
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

    let mut cmd = Command::new(&bin.ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(video_input_args(cfg))
        .args(video_output_args(cfg))
        // The null muxer: the encode is real, the container is not. Nothing is written to
        // disk, so a probe cannot fill a scratch directory or leave a file behind.
        .args(["-f", "null", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {} to measure {describe}", bin.ffmpeg.display()))?;

    let sink = child.stdin.take().context("the probe child's stdin is unavailable")?;
    let frame = probe_frame(cfg.source_size);
    // The bytes of one row of the frame: how far the pattern is rotated between frames.
    let stride = (cfg.source_size.0 as usize * 4).max(1);
    let frame_len = frame.len();
    let writer = std::thread::Builder::new()
        .name("encode-probe".into())
        .spawn(move || feed(sink, &frame, stride, budget))
        .context("spawning the probe's writer thread")?;

    // The whole call is bounded: the feeding window plus the drain ceiling. On expiry the
    // child is killed and reaped, which also breaks the writer's blocked write, so the join
    // below cannot park here.
    let waited = wait_with_deadline(&mut child, budget + drain);
    let fed = writer.join().map_err(|_| anyhow::anyhow!("the probe's writer thread panicked"))?;
    let (frames, window) = fed;
    let output = child.wait_with_output().context("collecting the probe child's output")?;

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
/// `None` — rather than a number — when there is nothing honest to compute:
///
/// * `frames == 0`: nothing was accepted, which is a failed probe, not a rate of zero;
/// * a window shorter than [`MIN_RATE_WINDOW`]: dividing by (nearly) zero yields infinity or
///   a number in the millions, and the caller would *declare* it as a frame rate. An absurd
///   duration is therefore refused instead of propagated.
///
/// The division is by the window that actually elapsed rather than by the nominal budget: a
/// window that overran (a blocked write, a frame that took longer than the deadline to be
/// accepted) would otherwise inflate the rate, and this number decides how fast the pipeline
/// declares itself to be running.
pub fn frames_per_second(frames: u64, window: Duration) -> Option<f64> {
    if frames == 0 || window < MIN_RATE_WINDOW {
        return None;
    }
    let fps = frames as f64 / window.as_secs_f64();
    fps.is_finite().then_some(fps)
}

/// Write `frame` to `sink` as fast as it is accepted, until `budget` elapses or the sink
/// stops taking it. Returns the frames accepted in the measured window, and how long that
/// window was.
///
/// The first frame is written but **not counted**, and the window starts when its write
/// completes. That write is the expensive one: it does not finish until ffmpeg has opened
/// the encoder *and* pulled those bytes through the pipe, so on a machine whose encoder takes
/// a few hundred milliseconds to initialise, counting it would report a rate below what the
/// machine sustains. Under-declaring is not a harmless error here — it makes the recording
/// run slower than the hardware can, for the life of the process.
///
/// The last frame may overrun the budget by its own write: a write that completes after the
/// deadline still means the encoder accepted that frame, and dropping it from the count would
/// bias the rate down. The overrun is therefore bounded by one frame's encode time, and the
/// caller's overall deadline ([`PROBE_DRAIN_TIMEOUT`]) is what bounds the call.
///
/// Each frame is written from a rotating offset, which costs no copy and means no two frames
/// are byte-identical — an encoder asked to encode the same picture 45 times would report a
/// rate no real content could reach (see [`probe_frame`]).
///
/// A sink that stops accepting frames (a dead child: the write fails with a broken pipe) ends
/// the loop instead of hanging, and dropping `sink` at the end of this function is the
/// child's EOF.
fn feed(mut sink: impl Write, frame: &[u8], stride: usize, budget: Duration) -> (u64, Duration) {
    let mut accepted: u64 = 0;
    let mut window = Duration::ZERO;
    let mut started: Option<Instant> = None;
    let mut offset = 0usize;

    loop {
        // One frame, in two slices, so the rotation costs no memcpy. `split_at` yields
        // `(frame[..offset], frame[offset..])`, so the halves are written tail-first: the
        // bytes the encoder receives are `frame[offset..] ++ frame[..offset]`, i.e. the
        // pattern rotated by `offset`. Writing them in the other order rebuilds the frame
        // byte for byte and every frame is the same picture again — which is exactly what
        // the rotation exists to prevent (found by
        // `the_feeder_writes_whole_frames_and_rotates_them`). Splitting one frame across two
        // writes is invisible to ffmpeg: the pipe is a byte stream, and it reassembles
        // exactly this many bytes into one frame.
        let (head, tail) = frame.split_at(offset);
        if sink.write_all(tail).is_err() || sink.write_all(head).is_err() {
            break;
        }
        offset = (offset + stride) % frame.len().max(1);

        match started {
            // The warm-up frame: written, deliberately not counted (see the doc above).
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
/// Two deliberate choices, both about not measuring a cheaper encoder than the one that will
/// record:
///
/// * **the real capture size.** Encode cost is dominated by the pixel count, so a small probe
///   frame would predict a rate several times too high for a 4K capture — the exact mistake
///   that produced issue #1's ~24fps-against-30 mismatch in the first place. One frame at
///   4K is 33.2 MB, the same magnitude as a frame the live pipeline already holds, and it is
///   the whole memory cost of this probe.
/// * **a detailed, moving pattern.** A flat (black) frame is a degenerate case for a software
///   encoder: it is nearly free to encode, and identical consecutive frames cost almost
///   nothing, so a black probe would report a rate that real content cannot hold. The pattern
///   is a deterministic pseudo-random field and [`feed`] rotates it between frames, so the
///   probe never flatters the encoder. On a hardware encoder this costs nothing and changes
///   nothing: its rate is a property of the resolution, not of the picture.
///
/// Deterministic on purpose: the same size always produces the same bytes, so two runs of the
/// probe are comparable (and a failure is reproducible).
fn probe_frame(size: (u32, u32)) -> Vec<u8> {
    let len = (size.0 as usize)
        .saturating_mul(size.1 as usize)
        .saturating_mul(4);
    let mut frame = vec![0u8; len];
    // xorshift64* — the point is a detailed pattern, not cryptography.
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

    /// A configuration at a small size — these tests are about the probe's arithmetic and
    /// its bounds, not about how fast a 4K encode is on the machine running them.
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
        assert_eq!(frames_per_second(45, Duration::from_millis(1500)), Some(30.0));
        // 100 frames in 4s is 25/s, not 100/1.
        assert_eq!(frames_per_second(100, Duration::from_secs(4)), Some(25.0));
    }

    /// The zero-duration case, and the absurd one: a rate computed by dividing by (nearly)
    /// nothing is not a measurement, and this number would be *declared* as the pipeline's
    /// frame rate. Refusing it is the point — an invented rate is worse than no rate.
    #[test]
    fn a_zero_or_absurd_duration_yields_no_rate_instead_of_a_huge_one() {
        assert_eq!(frames_per_second(30, Duration::ZERO), None);
        assert_eq!(frames_per_second(30, Duration::from_nanos(1)), None);
        assert_eq!(frames_per_second(30, Duration::from_micros(999)), None);
        assert_eq!(frames_per_second(0, Duration::from_secs(1)), None, "no frames is not 0fps");
        // At the floor the arithmetic is ordinary again.
        assert_eq!(frames_per_second(3, Duration::from_millis(1)), Some(3_000.0));
    }

    // ---- the probe against a real ffmpeg ----------------------------------------------

    /// The frame the probe sends is deterministic and not uniform — a flat frame would let a
    /// software encoder report a rate real content cannot hold.
    #[test]
    fn the_probe_frame_is_deterministic_and_has_detail_in_it() {
        let frame = probe_frame((16, 4));
        assert_eq!(frame.len(), 16 * 4 * 4);
        assert_eq!(frame, probe_frame((16, 4)), "the same size always yields the same bytes");
        let distinct: std::collections::HashSet<u8> = frame.iter().copied().collect();
        assert!(
            distinct.len() > 16,
            "a flat frame would be a degenerate case for a software encoder: {} distinct \
             byte values",
            distinct.len()
        );
    }

    /// Every frame written is a whole frame, and no two consecutive frames are identical —
    /// the rotation is what keeps an encoder from encoding the same picture 45 times and
    /// reporting a rate no content could reach.
    #[test]
    fn the_feeder_writes_whole_frames_and_rotates_them() {
        let frame = probe_frame((16, 4));
        let stride = 16 * 4;
        let mut sink: Vec<u8> = Vec::new();
        let (accepted, window) =
            feed(&mut sink, &frame, stride, Duration::from_millis(2));
        assert!(accepted >= 1, "a sink that always accepts must yield a count: {accepted}");
        assert!(window >= Duration::from_millis(2), "the window is the budget: {window:?}");
        assert_eq!(
            sink.len() % frame.len(),
            0,
            "{} bytes written is not a whole number of {} byte frames",
            sink.len(),
            frame.len()
        );
        let frames = sink.len() / frame.len();
        assert!(frames as u64 > accepted, "the warm-up frame was written too: {frames}");
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

    /// The real encoder, on this host, through the real probe. Needs `test-encoders` because
    /// that is what makes libx264 reachable off Windows (there is no other encoder here).
    ///
    /// What is asserted is what the probe *promises*: a plausible positive rate, and a call
    /// that finishes inside its budget plus a small margin rather than running away.
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

        assert!(m.fps > 0.0, "a real encoder must yield a positive rate: {m:?}");
        assert!(
            m.fps < 100_000.0,
            "64x48 libx264 cannot be anywhere near this fast, so the arithmetic is wrong: {m:?}"
        );
        assert_eq!(m.source_size, (64, 48), "the rate is about the size that was measured");
        assert_eq!(m.output_size, (64, 48), "and where it was written");
        assert!(m.frames > 0, "the frames that were counted are reported: {m:?}");
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

    /// A wedged child must be killed and reported rather than waited out. The deadline is
    /// what keeps startup bounded, and a *leaked* ffmpeg would sit on the user's machine for
    /// the rest of the session — so the writer thread has to end too, which it does as soon
    /// as the killed child closes the read end of the pipe.
    #[cfg(unix)]
    #[test]
    fn a_wedged_encoder_is_killed_and_reported_within_its_deadline() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("a temp dir");
        let stub = dir.path().join("ffmpeg");
        // `exec` so the child that is killed is the `sleep` itself: a shell that forked would
        // leave an orphan behind and this test would be measuring the wrong thing.
        std::fs::write(&stub, "#!/bin/sh\nexec sleep 120\n").expect("writing the stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("making the stub executable");
        let bin = FfmpegBinaries { ffmpeg: stub.clone(), ffprobe: stub };

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

    /// An encoder that cannot start must produce an error that names what happened — never a
    /// number. This is the "if the encoder dies, say so rather than guess" half.
    #[test]
    fn a_dead_encoder_is_an_error_not_a_rate() {
        let mut cfg = cfg(30);
        // The same shape the hardware path takes: a named encoder. A name ffmpeg does not
        // know stands in for a vendor encoder that cannot open its runtime (the measured
        // `h264_amf` / "DLL amfrt64.dll failed to open" case).
        cfg.software_encoder = Some("definitely_not_an_encoder");
        let err = measure_sustainable_fps(&ffmpeg(), &cfg, Duration::from_millis(200))
            .expect_err("a missing encoder cannot be measured");
        let msg = format!("{err:#}");
        assert!(msg.contains("definitely_not_an_encoder"), "ffmpeg's words are kept: {msg}");
        assert!(msg.contains("adapt_fps = false"), "the way out is named: {msg}");
    }
}
