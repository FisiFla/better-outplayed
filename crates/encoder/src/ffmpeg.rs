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
//! ## Threading
//!
//! Each input is drained by its own writer thread, fed by an unbounded `mpsc` channel,
//! so `submit_video`/`submit_audio` never block the caller. ffmpeg will not pull one
//! input far ahead of the other (its muxer buffers to interleave audio and video), so
//! writing both streams in bursts from the calling thread deadlocks as soon as either
//! sink's buffer fills. Decoupling the two writes lets the submits arrive in any order.

use crate::{EncodeConfig, Encoder};
use anyhow::{bail, Context, Result};
use localplay_capture::{AudioBuffer, Frame};
use localplay_media::FfmpegBinaries;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender};
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

/// Bytes queued by the caller, written to the child by a dedicated thread.
type WriterHandle = JoinHandle<std::io::Result<()>>;

pub struct FfmpegEncoder {
    child: Child,
    video_tx: Option<Sender<Vec<u8>>>,
    audio_tx: Option<Sender<Vec<u8>>>,
    video_writer: Option<WriterHandle>,
    audio_writer: Option<WriterHandle>,
    encoder_name: &'static str,
}

impl FfmpegEncoder {
    pub fn spawn(bin: &FfmpegBinaries, cfg: &EncodeConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.scratch_dir)
            .with_context(|| format!("creating {}", cfg.scratch_dir.display()))?;

        let pattern = cfg.scratch_dir.join("seg-%06d.mp4");
        let keyframe_secs = cfg.segment_ms as f64 / 1000.0;
        let gop = (cfg.fps as f64 * keyframe_secs).round().max(1.0) as u32;

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
            // Video input: raw BGRA frames on stdin.
            .args(["-f", "rawvideo", "-pix_fmt", "bgra"])
            // `-s` sizes the incoming rawvideo stream, so it must be the SOURCE size
            // (what the capture backend delivers), never the encode output size.
            .args(["-s", &format!("{}x{}", cfg.source_size.0, cfg.source_size.1)])
            .args(["-r", &cfg.fps.to_string()])
            .args(["-i", "pipe:0"])
            // Audio input: raw s16le PCM over loopback TCP. This is the transport that
            // works on Windows as well as here — see the module comment.
            .args(["-f", "s16le", "-ar", "48000", "-ac", "2", "-i", &audio_url]);
        // Frames arrive at `source_size`; when the caller asked for a different output
        // size, scale to it. Equal sizes add no filter, which avoids a pointless pass.
        if cfg.source_size != cfg.output_size {
            cmd.args(["-vf", &format!("scale={}:{}", cfg.output_size.0, cfg.output_size.1)]);
        }
        cmd.args(["-c:v", cfg.encoder_name()])
            .args(["-b:v", &format!("{}k", cfg.bitrate_kbps)])
            .args(["-g", &gop.to_string()])
            // Forced keyframes are what make segment boundaries cuttable (spec §6.3).
            .args([
                "-force_key_frames",
                &format!("expr:gte(t,n_forced*{keyframe_secs})"),
            ])
            .args(["-c:a", "aac", "-b:a", &format!("{}k", cfg.audio_bitrate_kbps)])
            .args(["-f", "segment"])
            .args(["-segment_time", &keyframe_secs.to_string()])
            .args(["-segment_format", "mp4"])
            .args(["-reset_timestamps", "1"])
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

        let (video_tx, video_rx) = mpsc::channel::<Vec<u8>>();
        let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>();

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
    fn submit_video(&mut self, frame: &Frame) -> Result<()> {
        let tx = self.video_tx.as_ref().context("encoder already finished")?;
        tx.send(frame.data.clone())
            .map_err(|_| anyhow::anyhow!("video writer thread has stopped"))
    }

    fn submit_audio(&mut self, audio: &AudioBuffer) -> Result<()> {
        let tx = self.audio_tx.as_ref().context("encoder already finished")?;
        tx.send(audio.data.clone())
            .map_err(|_| anyhow::anyhow!("audio writer thread has stopped"))
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
}

impl FfmpegEncoder {
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
    fn drain_stderr(&mut self, budget: Duration) -> String {
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
        rx.recv_timeout(budget).unwrap_or_default()
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
