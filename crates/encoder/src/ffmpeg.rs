//! One ffmpeg process, a pipe video input, a loopback-TCP audio input, segmented
//! MP4 output.
//!
//! Video arrives on `pipe:0` — the child's stdin, which `std::process` hands us as a
//! write end directly, so it needs no socket. Audio arrives on
//! `tcp://127.0.0.1:{port}`: we bind a listener on an ephemeral loopback port and
//! ffmpeg dials it, because for an input URL ffmpeg's tcp protocol is the CLIENT.
//!
//! ## Why audio does not come in on `pipe:1`
//!
//! Writing audio into the child's stdout (`-i pipe:1`) is a Unix-only trick, and an
//! accident of POSIX rather than a supported interface. ffmpeg's pipe protocol parses
//! the descriptor number out of the URL and then calls a plain CRT `read(fd, ...)`.
//! Windows opens descriptor 1 as write-only, so `read(1, ...)` fails with `EBADF` and
//! the audio input is dead on arrival. POSIX happens to permit reading fd 1, which is
//! precisely why the existing suite passed on macOS while the transport was broken on
//! the platform this project actually ships to. The trick was never portable.
//!
//! ## Why not Windows named pipes
//!
//! Named pipes would work on Windows, but the development host is macOS and cannot
//! exercise them: the transport would ship unverified on the very platform it exists
//! for. ffmpeg's `tcp://` protocol is supported on every platform, so loopback TCP
//! collapses this to ONE code path with no `#[cfg]` — and that path is genuinely
//! exercised by the macOS test suite, which is the only place it can be run here.
//!
//! ## This is IPC, not egress
//!
//! The listener is explicitly bound to `127.0.0.1` (never a wildcard, never a public
//! interface) and ffmpeg connects back to that same address, so neither the port nor
//! the peer can leave the host. The project's zero-egress rule (spec §7.3, enforced by
//! `crates/events/tests/no_egress.rs`) is about OUTBOUND traffic; two processes of
//! this application talking over the loopback interface are not egress. The accepted
//! connection's peer is checked to be a loopback address as well, so a stray
//! connection cannot silently feed the encoder.
//!
//! ## The media timeline is the wall clock, not the frame count
//!
//! A capture source delivers frames at whatever rate it manages — WGC hands over one
//! frame per compositor tick, which on a 36fps-ish delivery is *not* the configured
//! `encode.fps`. If the encoder assigned timestamps from a declared frame rate
//! (`-r 30`) the media timeline would advance at `frames / 30` while the wall clock
//! advanced at `frames / 36`, and the two would drift apart without bound: measured on
//! real hardware, 25.4s of wall clock produced 19.0s of media (919 frames, `span=19000ms`
//! against `need=28690ms`), so `trigger_ms + post_ms` could never be reached and every
//! hotkey press timed out with "the encoder produced no segment covering the trigger".
//!
//! Video input timestamps are therefore taken from the wall clock at the moment each
//! frame is *read* (`-use_wallclock_as_timestamps 1`), which is the arrival time of the
//! frame. Segments then cover wall-clock time — `segments * segment_time` is real
//! seconds — even when the delivery rate differs from the configured one, and a stall in
//! capture shows up as a gap in the timeline rather than as a slower-than-real-time clock.
//! The capture loop additionally rate-limits itself to `encode.fps` (see `FramePacer` in
//! the CLI), so in the ordinary case the two agree and no frames are wasted.
//!
//! The nominal rate is declared with the rawvideo demuxer's own `-framerate`, NOT with
//! the CLI's input `-r`. This is not cosmetic: `-r` before `-i` sets the CLI's notion of
//! an input frame rate, and ffmpeg then *re-stamps* every decoded frame onto a rigid
//! 1/fps grid, throwing the arrival time away — measured with `-debug_ts`, a pipe fed at
//! 15fps with wallclock stamps and input `-r 30` reached the muxer as pts 0, 1/30, 2/30,
//! … while the same pipe with `-framerate 30` reached it as 0, 0.100, 0.233, … i.e. the
//! real arrival times. `-r` silently undoes the whole fix, so it must not be used here.
//!
//! Audio is deliberately *not* stamped that way. Its input is raw PCM whose timeline is
//! already exact: s16le at 48kHz, muxed from the running sample count, gives 1/48000 s
//! per sample with no accumulation error and no dependence on when a block happened to
//! be read off the socket (and it is drained in full rather than rate-limited, because
//! dropping audio to pace video would desync the clip). Stamping audio from the wall
//! clock would replace that exact timeline with socket-arrival jitter. The two streams
//! are reconciled by the same clock the trigger uses — the capture loop's — so the small
//! drift between the audio device's crystal and the system clock is what remains, and
//! `MediaInfo::av_drift` measures it on every clip.
//!
//! ## Threading
//!
//! Each input is drained by its own writer thread, fed by a **bounded** `mpsc` channel,
//! so `submit_video`/`submit_audio` never block the caller. ffmpeg will not pull one
//! input far ahead of the other (its muxer buffers to interleave audio and video), so
//! writing both streams in bursts from the calling thread deadlocks as soon as either
//! sink's buffer fills. Decoupling the two writes lets the submits arrive in any order.
//!
//! The payload's buffer is **moved** into the channel, not cloned: `Frame`/`AudioBuffer`
//! are taken by value in the trait for exactly that reason. The clone this replaced
//! copied the whole pixel buffer on every frame — 3840x2160 BGRA is 33.2MB, ~1GB/s of
//! pure memcpy at 30fps — on the path that was already failing to keep up.

use crate::{EncodeConfig, Encoder};
use anyhow::{bail, Context, Result};
use localplay_capture::{AudioBuffer, Frame};
use localplay_media::FfmpegBinaries;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long the audio thread waits for ffmpeg to dial the loopback input before
/// giving up. ffmpeg connects while opening its inputs, well before it reads a frame
/// of video, so this only elapses when the child failed to start at all.
const AUDIO_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval while waiting for that connection. The listener is non-blocking, so
/// we re-check instead of sitting in a blocking `accept()` that could never return.
const AUDIO_CONNECT_POLL: Duration = Duration::from_millis(20);

/// How long a failure report waits for ffmpeg's stderr before giving up on including it.
/// The text is normally already sitting in the pipe buffer, so this is a ceiling on a
/// pathological case, not a delay on the ordinary one (see `drain_stderr`).
const STDERR_DRAIN_BUDGET: Duration = Duration::from_secs(1);

/// How long a writer failure waits for ffmpeg to become reapable, so its exit status and
/// stderr can be reported as the cause (see `FfmpegEncoder::ffmpeg_death`).
///
/// The broken pipe is noticed the moment ffmpeg closes its stdin, which can be a few
/// milliseconds before the process itself can be waited on. This is spent only on a path
/// that has already failed, and it is a deadline, not a guarantee: past it the error is
/// reported with the symptom alone.
const CHILD_DEATH_GRACE: Duration = Duration::from_millis(250);

/// Poll interval while waiting out that grace period.
const CHILD_DEATH_POLL: Duration = Duration::from_millis(10);

/// How many video frames may sit in the queue between the capture loop and the writer
/// thread before submits start being dropped.
///
/// The queue is bounded because an unbounded one grows without limit whenever ffmpeg
/// reads slower than capture produces — the opposite of the project's flat-RAM principle,
/// and it fails by exhausting memory rather than by losing frames.
///
/// The number is small on purpose, and its size is dominated by the frame, not by the
/// count: at 3840x2160 BGRA one frame is 3840*2160*4 = 33_177_600 B (~33.2 MB), so four
/// queued frames are ~133 MB, and with the fifth frame the writer thread holds while
/// blocked inside `write()` the encoder's worst case is ~166 MB. The slack that buys is
/// also small on purpose: at 30fps a frame is 33 ms, so four frames ride out ~133 ms of
/// ffmpeg not reading — long enough to cover a segment being finalised (moov written,
/// file closed, next one opened) without dropping anything, far short of letting a
/// wedged encoder accumulate a gigabyte.
const VIDEO_QUEUE_FRAMES: usize = 4;

/// How many 10 ms audio blocks may be queued before audio submits start being dropped.
///
/// Blocks are 1920 B at 48kHz stereo s16le, so this is ~61 KB — 320 ms of audio, ~1600x
/// cheaper per millisecond of slack than the video queue. Audio is given more slack than
/// video for that reason: a dropped block is a hole in the sound, and the memory saved by
/// trimming this number would be noise.
const AUDIO_QUEUE_BLOCKS: usize = 32;

/// Bytes queued by the caller, written to the child by a dedicated thread.
type WriterHandle = JoinHandle<std::io::Result<()>>;

/// Seconds per scratch segment — which is also the forced-keyframe interval, and therefore
/// what makes a clip a lossless concatenation of whole segments (spec §6.3).
pub fn keyframe_seconds(cfg: &EncodeConfig) -> f64 {
    cfg.segment_ms as f64 / 1000.0
}

/// The GOP that puts a keyframe at every segment boundary.
fn gop_frames(cfg: &EncodeConfig) -> u32 {
    (cfg.fps as f64 * keyframe_seconds(cfg)).round().max(1.0) as u32
}

/// The ffmpeg arguments that declare the **video input and its rate**.
///
/// Shared by the two places that drive this encoder — the long-lived child that records
/// ([`FfmpegEncoder::spawn`]) and the startup throughput probe that measures what the
/// machine can sustain before it is asked to record anything
/// (`crate::throughput::measure_sustainable_fps`) — because a measurement of a *different*
/// ffmpeg invocation is not a measurement of this one. The geometry, the pixel format and
/// above all the declared rate have to be the same on both sides for the number the probe
/// returns to mean anything about the recording that follows.
///
/// `-framerate` is the rawvideo demuxer's own option and deliberately NOT the CLI's input
/// `-r`: `-r` makes ffmpeg treat the input as constant-rate and re-stamp every frame onto a
/// rigid 1/fps grid, which discards the arrival timestamp the next option exists to record
/// (see the module comment for the measurement). Both spellings declare the same nominal
/// rate to the rawvideo demuxer; only this one leaves the real timestamps alone.
pub fn video_input_args(cfg: &EncodeConfig) -> Vec<String> {
    let mut args: Vec<String> = ["-f", "rawvideo", "-pix_fmt", "bgra"].map(str::to_string).to_vec();
    // `-s` sizes the incoming rawvideo stream, so it must be the SOURCE size (what the
    // capture backend delivers), never the encode output size.
    args.extend(["-s".to_string(), format!("{}x{}", cfg.source_size.0, cfg.source_size.1)]);
    // The nominal rate: what the pipeline declares the stream to be. It is one number for
    // both consumers — this argument and the pacer that feeds the pipe — see
    // `localplay_recorder::FpsDecision`.
    args.extend(["-framerate".to_string(), cfg.fps.to_string()]);
    // Timestamps come from the moment each frame is read, i.e. its arrival time, not from a
    // declared frame rate. This is what keeps the media timeline glued to the wall clock:
    // `segments * segment_time` stays real seconds even when capture delivers at a rate
    // other than `cfg.fps`, so the post-roll the hotkey waits for is actually reached.
    // Video only — audio's timeline is the exact 48kHz sample count and must not be
    // jittered by socket arrival (module comment: "The media timeline is the wall clock,
    // not the frame count").
    args.extend(["-use_wallclock_as_timestamps".to_string(), "1".to_string()]);
    args.extend(["-i".to_string(), "pipe:0".to_string()]);
    args
}

/// The ffmpeg arguments that describe **the encoder for that video input**: the scale
/// filter when the output differs from the capture, the codec, its bitrate and its keyframe
/// schedule.
///
/// Shared with the throughput probe for the same reason as [`video_input_args`]: the probe
/// has to pay for the work the recording pays for. It writes to the null muxer, so the
/// forced keyframes and the scale cost it exactly what they cost the segmenter.
pub fn video_output_args(cfg: &EncodeConfig) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    // Frames arrive at `source_size`; when the caller asked for a different output size,
    // scale to it. Equal sizes add no filter, which avoids a pointless pass.
    if cfg.source_size != cfg.output_size {
        args.extend([
            "-vf".to_string(),
            format!("scale={}:{}", cfg.output_size.0, cfg.output_size.1),
        ]);
    }
    args.extend(["-c:v".to_string(), cfg.encoder_name().to_string()]);
    args.extend(["-b:v".to_string(), format!("{}k", cfg.bitrate_kbps)]);
    args.extend(["-g".to_string(), gop_frames(cfg).to_string()]);
    // Forced keyframes are what make segment boundaries cuttable (spec §6.3).
    args.extend([
        "-force_key_frames".to_string(),
        format!("expr:gte(t,n_forced*{})", keyframe_seconds(cfg)),
    ]);
    args
}

pub struct FfmpegEncoder {
    child: Child,
    video_tx: Option<SyncSender<Vec<u8>>>,
    audio_tx: Option<SyncSender<Vec<u8>>>,
    video_writer: Option<WriterHandle>,
    audio_writer: Option<WriterHandle>,
    encoder_name: &'static str,
    /// The geometry the rawvideo pipe was declared with (`-s {w}x{h}`), reported back to
    /// the caller so a frame of any other size can be refused instead of being sliced
    /// into the pipe at the wrong stride (see `Encoder::source_size`).
    source_size: (u32, u32),
    /// The rate the rawvideo pipe was declared with (`-framerate {fps}`), reported back so
    /// the caller that paces capture can pace to exactly the number the child was told
    /// (see `Encoder::input_fps`).
    input_fps: u32,
    /// Frames dropped because a queue was full. Atomics because the count is written on
    /// the submitting thread and read through `&self` (see `Encoder::dropped_frames`).
    dropped_video: AtomicU64,
    dropped_audio: AtomicU64,
    /// Text already read out of ffmpeg's stderr, which a pipe can only give up once
    /// (see [`FfmpegEncoder::drain_stderr`]). Two different reports can want it — the
    /// writer failure that explains a dead encoder, and [`Encoder::finish`] — and the
    /// second one must not be left saying nothing.
    drained_stderr: Option<String>,
}

impl FfmpegEncoder {
    pub fn spawn(bin: &FfmpegBinaries, cfg: &EncodeConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.scratch_dir)
            .with_context(|| format!("creating {}", cfg.scratch_dir.display()))?;

        let pattern = cfg.scratch_dir.join("seg-%06d.mp4");
        let keyframe_secs = keyframe_seconds(cfg);

        // Audio comes in over loopback TCP. Bind before spawning the child: the port
        // has to be in the argument list, and `:0` makes the OS pick a free one, which
        // we read back from the socket rather than guessing (racing another process
        // for a fixed port is exactly what this avoids).
        let audio_listener = TcpListener::bind("127.0.0.1:0")
            .context("binding the audio input listener on 127.0.0.1")?;
        let audio_port = audio_listener
            .local_addr()
            .context("reading the audio input listener's port")?
            .port();
        let audio_url = format!("tcp://127.0.0.1:{audio_port}");

        let mut cmd = Command::new(&bin.ffmpeg);
        cmd
            // `-nostdin` keeps ffmpeg from consuming our stdin for interactive
            // commands, which would steal raw video frames.
            .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            // Video input: raw BGRA frames on stdin, at the declared rate. Shared with the
            // startup throughput probe — see [`video_input_args`], which is also where the
            // `-framerate`-not-`-r` rule and the arrival timestamps are documented.
            .args(video_input_args(cfg))
            // Audio input: raw s16le PCM over loopback TCP. This is the transport that
            // works on Windows as well as here — see the module comment.
            .args(["-f", "s16le", "-ar", "48000", "-ac", "2", "-i", &audio_url])
            // The encoder for that video input: scale (when asked for), codec, bitrate and
            // the keyframe schedule. Shared with the probe for the same reason.
            .args(video_output_args(cfg))
            .args(["-c:a", "aac", "-b:a", &format!("{}k", cfg.audio_bitrate_kbps)])
            .args(["-f", "segment"])
            .args(["-segment_time", &keyframe_secs.to_string()])
            .args(["-segment_format", "mp4"])
            // Each segment starts at zero, which is what the concat at clip time relies
            // on (spec §6.3): every segment is a self-contained unit starting at t=0.
            .args(["-reset_timestamps", "1"])
            // Continue the numbering instead of restarting it. ffmpeg's segment muxer
            // supports this as `segment_start_number`; plain `-start_number` is *not* an
            // option of this muxer and is silently ignored (measured: with
            // `-start_number 5` the first file was still `seg-000000.mp4`), which would
            // leave the encoder overwriting files the adopted ledger still names.
            .args(["-segment_start_number", &cfg.start_number.to_string()])
            .arg(&pattern)
            .stdin(Stdio::piped())
            // Nothing is expected on stdout any more (it used to carry the audio
            // pipe). Route it to the null device so ffmpeg can never write into our
            // own stdout, where a stray byte would corrupt a caller's output.
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "spawning {} (requested encoder: {})",
                bin.ffmpeg.display(),
                cfg.encoder_name()
            )
        })?;

        // Video goes to the child's stdin; the audio listener is moved into the audio
        // thread, which accepts ffmpeg's connection there.
        let video_in = child.stdin.take().context("child stdin unavailable")?;

        let (video_tx, video_rx) = mpsc::sync_channel::<Vec<u8>>(VIDEO_QUEUE_FRAMES);
        let (audio_tx, audio_rx) = mpsc::sync_channel::<Vec<u8>>(AUDIO_QUEUE_BLOCKS);

        let video_writer = std::thread::Builder::new()
            .name("ffmpeg-video-in".into())
            .spawn(move || pump(video_rx, video_in))
            .context("spawning video writer thread")?;
        let audio_writer = std::thread::Builder::new()
            .name("ffmpeg-audio-in".into())
            .spawn(move || pump_audio(audio_listener, audio_rx, audio_port))
            .context("spawning audio writer thread")?;

        Ok(Self {
            child,
            video_tx: Some(video_tx),
            audio_tx: Some(audio_tx),
            video_writer: Some(video_writer),
            audio_writer: Some(audio_writer),
            encoder_name: cfg.encoder_name(),
            source_size: cfg.source_size,
            input_fps: cfg.fps,
            dropped_video: AtomicU64::new(0),
            dropped_audio: AtomicU64::new(0),
            drained_stderr: None,
        })
    }
}

/// Copy queued chunks into a sink until the sender half is dropped, then close it.
/// Returning ends the function, dropping `sink` and signalling EOF to ffmpeg.
fn pump<W: Write>(rx: mpsc::Receiver<Vec<u8>>, mut sink: W) -> std::io::Result<()> {
    for chunk in rx {
        sink.write_all(&chunk)?;
    }
    sink.flush()
}

/// Accept ffmpeg's connection to the audio input, then stream the queued PCM into it.
///
/// Dropping the socket at the end closes it, which is ffmpeg's EOF on the audio input.
fn pump_audio(
    listener: TcpListener,
    rx: mpsc::Receiver<Vec<u8>>,
    port: u16,
) -> std::io::Result<()> {
    let stream = accept_within(&listener, port, AUDIO_CONNECT_TIMEOUT)?;
    // PCM arrives in small blocks (10ms = 1920 bytes at 48kHz stereo s16le) and ffmpeg
    // is reading them in real time, so Nagle would add nothing but latency here.
    stream.set_nodelay(true).map_err(|e| {
        std::io::Error::new(e.kind(), format!("disabling Nagle on the audio input socket: {e}"))
    })?;
    pump(rx, stream)
}

/// Accept one connection, giving up after `timeout`.
///
/// The listener is switched to non-blocking and polled rather than parked in a
/// blocking `accept()`. If ffmpeg fails to start — bad arguments, a missing encoder, a
/// child that exited on its own — nothing ever dials the port, and an indefinite
/// `accept()` would hang `finish()` for no stated reason. Polling turns that into a
/// message naming the port and the deadline. `WouldBlock` is the only error that means
/// "not yet"; anything else is a real failure and is returned as-is.
fn accept_within(
    listener: &TcpListener,
    port: u16,
    timeout: Duration,
) -> std::io::Result<TcpStream> {
    listener.set_nonblocking(true).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("making the audio input listener non-blocking: {e}"),
        )
    })?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, peer)) => {
                // Belt and braces for the zero-egress note above: the listener is bound
                // to loopback, so a non-loopback peer would mean our assumption about
                // this socket is wrong, and PCM would be going somewhere unexpected.
                if !peer.ip().is_loopback() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("audio input connection from non-loopback peer {peer}"),
                    ));
                }
                // The listener is non-blocking SOLELY so this poll loop can enforce a
                // connect deadline; that has no business leaking into the stream we
                // hand to the pump. BSD-derived platforms (macOS included) propagate
                // O_NONBLOCK from the listener to the socket `accept()` returns, while
                // Linux does not — so without this line the accepted socket is
                // non-blocking only on the very host this suite runs on. A
                // non-blocking socket makes `write_all` fail with `WouldBlock`
                // (`EAGAIN`, os error 35) the instant ffmpeg's receive buffer fills,
                // which is a race on how fast the pump fills it: the intermittent
                // failure this line fixes. A blocking accepted socket is the correct
                // design here: the pump runs on its own dedicated thread fed by a
                // bounded channel, so the backpressure of a blocking write can still
                // never propagate to `submit_audio` on the caller's thread: the submit
                // drops the block instead of waiting for room (see `enqueue_or_drop`).
                stream.set_nonblocking(false).map_err(|e| {
                    std::io::Error::new(
                        e.kind(),
                        format!("making the accepted audio input socket blocking: {e}"),
                    )
                })?;
                return Ok(stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "ffmpeg never connected to the audio input on 127.0.0.1:{port} \
                             within {timeout:?}"
                        ),
                    ));
                }
                std::thread::sleep(AUDIO_CONNECT_POLL);
            }
            Err(e) => return Err(e),
        }
    }
}

impl Encoder for FfmpegEncoder {
    /// Queue the frame for the rawvideo pipe, or drop it if the queue is full.
    ///
    /// The frame is taken by value and its pixel buffer is moved into the queue — that
    /// is what the ownership in the trait signature is for. The clone this replaces
    /// copied 3840x2160 BGRA (33.2MB) per frame, ~1GB/s of pure memcpy at 30fps on the
    /// path that is failing to keep up.
    ///
    /// `try_send` rather than `send`, and a drop rather than an error, because this is a
    /// live capture: this call sits in the same loop that has to keep pulling frames off
    /// the capture backend, so blocking here would take the latency budget from a stream
    /// that cannot be paused, and failing would stop recording because the GPU fell a few
    /// milliseconds behind. Dropping the frame is the correct behaviour for a live
    /// stream — the timeline is the wall clock (module comment), so a missing frame is a
    /// brief repeat of its predecessor, not a shift of everything after it.
    ///
    /// The count is what stops the drop from being silent: `Encoder::dropped_frames`
    /// reports it and the CLI logs it, so a soak sees the loss instead of inferring it.
    ///
    /// A disconnected channel is a different matter — the writer thread is gone, ffmpeg
    /// is not reading, and nothing submitted afterwards can be encoded, so that is an
    /// error rather than a drop. That error carries ffmpeg's own exit status and stderr
    /// when they are known by then, because "video writer thread has stopped" is the
    /// symptom, not the cause ([`FfmpegEncoder::explain_dead_writer`]).
    fn submit_video(&mut self, frame: Frame) -> Result<()> {
        // The frame's geometry is checked by the caller (`pump_once_counted`), which is
        // the only place that holds both the frame and the pipe's declared size.
        let queued = match self.video_tx.as_ref() {
            Some(tx) => enqueue_or_drop(tx, frame.data, &self.dropped_video, "video"),
            None => bail!("encoder already finished"),
        };
        queued.map_err(|e| self.explain_dead_writer(e))
    }

    /// Queue an audio block, or drop it if the queue is full. Same reasoning as
    /// [`Encoder::submit_video`]; the counter is `Encoder::dropped_audio_blocks`.
    fn submit_audio(&mut self, audio: AudioBuffer) -> Result<()> {
        let queued = match self.audio_tx.as_ref() {
            Some(tx) => enqueue_or_drop(tx, audio.data, &self.dropped_audio, "audio"),
            None => bail!("encoder already finished"),
        };
        queued.map_err(|e| self.explain_dead_writer(e))
    }

    fn finish(&mut self) -> Result<()> {
        // Dropping the senders ends each pump loop, closing the video pipe and the
        // audio socket — which is what tells ffmpeg the inputs have ended.
        self.video_tx.take();
        self.audio_tx.take();
        // Join both writers before reporting: the audio thread may still be waiting
        // out its connect deadline, and abandoning it would leave a listener that a
        // later connection could reach after this encoder is done.
        let video_res = join_writer(self.video_writer.take(), "video");
        let audio_res = join_writer(self.audio_writer.take(), "audio");

        // The connect deadline is the one failure that leaves ffmpeg alive but silent:
        // it never opened the audio input, so it never read video either, and waiting
        // on it below would block forever. Kill it, then report the deadline along
        // with whatever it managed to log.
        if let Err(audio_err) = &audio_res {
            if is_connect_deadline(audio_err) {
                let _ = self.child.kill();
                let status = self.child.wait().context("reaping ffmpeg")?;
                let stderr = self.drain_stderr(STDERR_DRAIN_BUDGET);
                bail!(
                    "{audio_err:#}; ffmpeg status after the deadline was {status}, stderr: {}",
                    stderr.trim()
                );
            }
        }

        // ffmpeg's own exit status and stderr explain a failed start (a bad encoder
        // name, for instance); the writer threads only ever see the symptom — a broken
        // pipe. Report the cause first, and fall back to the writer errors.
        let status = self.child.wait().context("waiting for ffmpeg")?;
        if !status.success() {
            let stderr = self.drain_stderr(STDERR_DRAIN_BUDGET);
            bail!(
                "ffmpeg exited with {status} using encoder '{}': {}",
                self.encoder_name,
                stderr.trim()
            );
        }
        video_res?;
        audio_res?;
        Ok(())
    }

    fn active_encoder(&self) -> &'static str {
        self.encoder_name
    }

    fn source_size(&self) -> (u32, u32) {
        self.source_size
    }

    fn input_fps(&self) -> u32 {
        self.input_fps
    }

    fn dropped_frames(&self) -> u64 {
        self.dropped_video.load(Ordering::Relaxed)
    }

    fn dropped_audio_blocks(&self) -> u64 {
        self.dropped_audio.load(Ordering::Relaxed)
    }
}

impl FfmpegEncoder {
    /// Complete a writer failure with ffmpeg's own exit status and stderr, if it has died.
    ///
    /// The writer thread can only report the symptom: its `write` to the child's stdin
    /// failed, so it stopped, so the channel is disconnected. It does not own the child,
    /// and the fact that arrives at the caller is "…writer thread has stopped" — which is
    /// exactly the unhelpful message a Windows capture produced when `h264_amf` could not
    /// start (`Error: video writer thread has stopped`, with the real cause — a missing
    /// `amfrt64.dll` — only ever in ffmpeg's stderr). This encoder owns the child, so it
    /// can add the cause.
    ///
    /// Returns the error untouched when the child has not exited yet. That is a real
    /// possibility (the pipe can break before the process is reapable) and inventing a
    /// cause would be worse than reporting the symptom honestly.
    fn explain_dead_writer(&mut self, err: anyhow::Error) -> anyhow::Error {
        match self.ffmpeg_death() {
            Some(cause) => anyhow::Error::msg(format!("{err}; {cause}")),
            None => err,
        }
    }

    /// ffmpeg's exit status and stderr, when it has exited.
    ///
    /// A short bounded grace period is spent waiting for the exit first. The broken pipe
    /// is observed the instant ffmpeg closes its stdin, which can precede the moment the
    /// process becomes reapable, and the whole point is to catch the cause. The wait only
    /// happens on a path that has already failed — the run is ending either way — and it
    /// is bounded, so a child that survives the grace period still produces an error.
    ///
    /// `try_wait` reaps the child; a later [`Encoder::finish`] still gets the same status
    /// from `Child::wait`, which returns the stored one.
    fn ffmpeg_death(&mut self) -> Option<String> {
        let deadline = Instant::now() + CHILD_DEATH_GRACE;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(CHILD_DEATH_POLL),
                _ => return None,
            }
        };
        let stderr = self.drain_stderr(STDERR_DRAIN_BUDGET);
        Some(format!(
            "ffmpeg exited with {status} using encoder '{}': {}",
            self.encoder_name,
            stderr.trim()
        ))
    }

    /// Everything ffmpeg wrote to stderr, but never at the cost of waiting longer than
    /// `budget` for it.
    ///
    /// A plain `read_to_string` blocks until EVERY write end of the pipe is closed.
    /// Normally the child holds the only one, so the read ends where the child does —
    /// but a descendant that inherited the pipe and outlives a killed child keeps it
    /// open, and the read would then hang the error path (measured: a stand-in that
    /// forked a sleeper stalled the report for its whole 60s). Draining on its own
    /// thread and taking whatever arrived keeps the report honest and bounded; on the
    /// ordinary path the text is already buffered and this returns at once.
    ///
    /// The text is remembered because a pipe can only be drained once, and more than one
    /// report wants it: the writer failure that explains a dead encoder, and
    /// [`Encoder::finish`]'s own failure. Without the cache the second one would print
    /// nothing where ffmpeg's reason should be.
    fn drain_stderr(&mut self, budget: Duration) -> String {
        if let Some(text) = &self.drained_stderr {
            return text.clone();
        }
        let Some(mut stderr) = self.child.stderr.take() else {
            return String::new();
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            let _ = tx.send(text);
        });
        let text = rx.recv_timeout(budget).unwrap_or_default();
        self.drained_stderr = Some(text.clone());
        text
    }
}

/// Whether a writer error is the audio thread's connect deadline.
///
/// `accept_within` is the only place in this module that raises `TimedOut`, and the
/// audio thread is the only thread that calls it, so the error kind is the signal.
fn is_connect_deadline(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io| io.kind() == std::io::ErrorKind::TimedOut)
}

/// Hand a payload to a writer thread, or drop it if the queue is full.
///
/// `try_send` rather than `send`, and a drop rather than an error, because this is called
/// from the live capture path: blocking here would stall the loop that has to keep pulling
/// frames off the capture backend, and failing would stop recording because the GPU fell a
/// few milliseconds behind. Dropping is the correct behaviour for a live stream — the
/// media timeline is wall-clock (module comment), so a missing payload is a brief repeat
/// of its predecessor rather than a shift of everything after it. The counter is what
/// stops the drop from being silent: it is reported by `Encoder::dropped_frames` /
/// `Encoder::dropped_audio_blocks` and logged by the CLI.
///
/// A disconnected channel is a different matter — the writer thread is gone, ffmpeg is not
/// reading, and nothing submitted afterwards can be encoded, so that is an error.
fn enqueue_or_drop<T>(tx: &SyncSender<T>, item: T, dropped: &AtomicU64, what: &str) -> Result<()> {
    match tx.try_send(item) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => {
            dropped.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Err(TrySendError::Disconnected(_)) => bail!("{what} writer thread has stopped"),
    }
}

fn join_writer(handle: Option<WriterHandle>, what: &str) -> Result<()> {
    let Some(handle) = handle else { return Ok(()) };
    match handle.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            Err(anyhow::Error::new(e)).with_context(|| format!("writing {what} to ffmpeg"))
        }
        Err(_) => bail!("{what} writer thread panicked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configuration the argument builders can be exercised with. Small on purpose: these
    /// tests are about the argument *list*, not about encoding anything.
    fn args_cfg(fps: u32, segment_ms: u64) -> EncodeConfig {
        let mut cfg = EncodeConfig::for_tests_software(
            crate::VideoCodec::H264,
            1280,
            720,
            fps,
            ".".into(),
            segment_ms,
        );
        cfg.bitrate_kbps = 20_000;
        cfg
    }

    /// The rate in the child's argument list is `EncodeConfig::fps`, and nothing else. This
    /// is the number the pacer is built from as well (see `Encoder::input_fps`): one value,
    /// two consumers, which is the fix for issues #1/#2 — so a change that makes ffmpeg
    /// declare a different rate from the one the pipeline paces to has to fail here.
    #[test]
    fn the_declared_rate_is_the_configurations_fps_and_it_is_declared_as_framerate() {
        for fps in [10u32, 24, 30, 60] {
            let cfg = args_cfg(fps, 1000);
            let args = video_input_args(&cfg);
            let at = |flag: &str| {
                let i = args
                    .iter()
                    .position(|a| a == flag)
                    .unwrap_or_else(|| panic!("{flag} missing from {args:?}"));
                args.get(i + 1).cloned().unwrap_or_default()
            };
            assert_eq!(at("-framerate"), fps.to_string(), "the declared rate: {args:?}");
            assert_eq!(at("-s"), "1280x720", "the rawvideo pipe is the SOURCE size: {args:?}");
            assert!(
                !args.iter().any(|a| a == "-r"),
                "`-r` before `-i` re-stamps every frame onto a rigid grid and undoes the \
                 arrival timestamps this option records (measured; see the module comment): \
                 {args:?}"
            );
            assert_eq!(at("-use_wallclock_as_timestamps"), "1", "{args:?}");
        }
    }

    /// The scale filter is added only when the output size differs, and the GOP follows the
    /// declared rate — both of which the throughput probe inherits, because it builds its
    /// argument list from these same two functions.
    #[test]
    fn the_output_arguments_scale_only_when_asked_and_size_the_gop_from_the_rate() {
        let same = args_cfg(30, 1000);
        assert!(
            !video_output_args(&same).iter().any(|a| a == "-vf"),
            "equal sizes must not add a pointless scale pass"
        );

        let mut scaled = args_cfg(30, 1000);
        scaled.output_size = (1920, 1080);
        let args = video_output_args(&scaled);
        let vf = args.iter().position(|a| a == "-vf").expect("a scale filter");
        assert_eq!(args[vf + 1], "scale=1920:1080");
        assert!(args.contains(&"-c:v".to_string()));

        // One keyframe per segment: at 30fps and 1s segments the GOP is 30, at 2s it is 60.
        let gop = |cfg: &EncodeConfig| {
            let args = video_output_args(cfg);
            let i = args.iter().position(|a| a == "-g").expect("-g");
            args[i + 1].parse::<u32>().expect("a GOP number")
        };
        assert_eq!(gop(&args_cfg(30, 1000)), 30);
        assert_eq!(gop(&args_cfg(30, 2000)), 60);
        assert_eq!(gop(&args_cfg(24, 1000)), 24);
    }

    #[test]
    fn a_payload_that_fits_is_queued_and_not_counted_as_dropped() {
        let (tx, rx) = mpsc::sync_channel::<u32>(1);
        let dropped = AtomicU64::new(0);

        enqueue_or_drop(&tx, 7, &dropped, "video").expect("a free slot must accept the payload");

        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        assert_eq!(rx.try_recv().expect("the payload is queued"), 7);
    }

    #[test]
    fn a_full_queue_drops_the_payload_and_counts_it_instead_of_blocking_or_failing() {
        // Capacity 1, and nothing drains it: this is the "encoder cannot keep up" state.
        let (tx, rx) = mpsc::sync_channel::<u32>(1);
        let dropped = AtomicU64::new(0);

        enqueue_or_drop(&tx, 1, &dropped, "video").expect("a free slot accepts the payload");
        // These must return immediately and successfully — `send` here would block the
        // capture loop forever, and an error would stop the recording.
        enqueue_or_drop(&tx, 2, &dropped, "video").expect("a full queue drops, it does not fail");
        enqueue_or_drop(&tx, 3, &dropped, "video").expect("a full queue drops, it does not fail");

        assert_eq!(dropped.load(Ordering::Relaxed), 2, "both refused payloads are counted");
        assert_eq!(
            rx.try_recv().expect("the queued payload is still there"),
            1,
            "the queued payload is untouched"
        );
        assert!(rx.try_recv().is_err(), "the dropped payloads were not smuggled in behind it");
    }

    #[test]
    fn a_stopped_writer_thread_is_an_error_not_a_drop() {
        let (tx, rx) = mpsc::sync_channel::<u32>(1);
        let dropped = AtomicU64::new(0);
        drop(rx);

        let err = enqueue_or_drop(&tx, 1, &dropped, "video")
            .expect_err("nothing can be encoded once the writer thread is gone");
        assert!(
            err.to_string().contains("video writer thread has stopped"),
            "the error must name the dead writer: {err}"
        );
        assert_eq!(dropped.load(Ordering::Relaxed), 0, "a failure is not a drop");
    }

    /// A pipe can only be read once, and two reports want ffmpeg's words: the writer
    /// failure that explains a dead encoder, and the failure [`Encoder::finish`] raises
    /// afterwards. The second one must not be left with nothing — that would move the
    /// unexplained failure from one place to another.
    #[cfg(unix)]
    #[test]
    fn stderr_is_remembered_so_a_second_report_is_not_left_empty() {
        let child = Command::new("/bin/sh")
            .args(["-c", "echo 'DLL amfrt64.dll failed to open' >&2"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // The child is written by hand here because `spawn` needs a real ffmpeg and an
        // audio listener; this is the smallest thing that has a child with stderr.
        let mut encoder = FfmpegEncoder {
            child,
            video_tx: None,
            audio_tx: None,
            video_writer: None,
            audio_writer: None,
            encoder_name: "h264_amf",
            source_size: (320, 240),
            input_fps: 30,
            dropped_video: AtomicU64::new(0),
            dropped_audio: AtomicU64::new(0),
            drained_stderr: None,
        };
        encoder.child.wait().unwrap();

        let first = encoder.drain_stderr(Duration::from_secs(1));
        assert!(first.contains("DLL amfrt64.dll failed to open"), "got: {first:?}");
        let second = encoder.drain_stderr(Duration::from_secs(1));
        assert_eq!(second, first, "the second report gets the same words, not nothing");
    }

    /// The message a Windows capture produced was `Error: video writer thread has stopped`
    /// while the real cause — an `h264_amf` that could not initialise — sat in ffmpeg's
    /// stderr. A dead writer must now carry the child's own exit status and words.
    ///
    /// The stand-in child is a script that fails the way a hardware encoder does on a
    /// machine with no vendor runtime: it says why on stderr and exits non-zero. No GPU is
    /// needed, and the path under test — a pump thread noticing the broken pipe, then the
    /// submitter asking the child what happened — is the one the real encoder uses.
    #[cfg(unix)]
    #[test]
    fn a_dead_writer_reports_ffmpegs_exit_status_and_stderr() {
        use crate::VideoCodec;
        use localplay_capture::{Frame, PixelFormat};
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("ffmpeg");
        std::fs::write(&stub, "#!/bin/sh\necho 'DLL amfrt64.dll failed to open' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = FfmpegBinaries { ffmpeg: stub.clone(), ffprobe: stub };
        let cfg = EncodeConfig::for_tests_software(
            VideoCodec::H264,
            64,
            64,
            30,
            dir.path().to_path_buf(),
            1_000,
        );
        // Spawning succeeds: the child starts and then fails, which is exactly the shape
        // of an encoder that cannot initialise.
        let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).unwrap();

        // The channel only disconnects once the pump thread *tries* to write into the
        // dead child and fails, so submit until that happens; each attempt is an
        // independent frame the encoder may accept first.
        let frame = || Frame {
            data: vec![0u8; 64 * 64 * 4],
            pts: Duration::ZERO,
            width: 64,
            height: 64,
            format: PixelFormat::Bgra8,
        };
        let mut failure = None;
        for _ in 0..50 {
            match encoder.submit_video(frame()) {
                Ok(()) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => {
                    failure = Some(e.to_string());
                    break;
                }
            }
        }

        let err = failure.expect("a payload cannot be encoded once the writer thread is gone");
        assert!(err.contains("video writer thread has stopped"), "the symptom survives: {err}");
        assert!(
            err.contains("DLL amfrt64.dll failed to open"),
            "ffmpeg's own words are the point, and they were only in its stderr: {err}"
        );
        assert!(err.contains("exit status"), "and so is its exit status: {err}");
    }
}
