//! One ffmpeg process, two pipe inputs, segmented MP4 output.
//!
//! Video arrives on `pipe:0` (stdin). Audio arrives on `pipe:1` (stdout) — we write
//! into the child's stdout. This avoids needing an extra inherited descriptor, which
//! is awkward to set up portably with `std::process`.
//!
//! Each pipe is drained by its own writer thread. ffmpeg will not pull one input far
//! ahead of the other (its muxer buffers to interleave audio and video), so writing
//! both streams in bursts from the calling thread deadlocks as soon as either pipe's
//! buffer fills. Decoupling the two writes lets `submit_video`/`submit_audio` accept
//! bursts in any order.

use crate::{EncodeConfig, Encoder};
use anyhow::{bail, Context, Result};
use localplay_capture::{AudioBuffer, Frame};
use localplay_media::FfmpegBinaries;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;

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

        // The audio input lives on the child's stdout. `std::process` only hands us
        // that pipe as a read end (`ChildStdout`), so we build our own pipe and give
        // its read end to the child; the parent keeps the write end.
        let (audio_reader, audio_writer) = std::io::pipe()?;

        let mut cmd = Command::new(&bin.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            // Video input: raw BGRA frames on stdin.
            .args(["-f", "rawvideo", "-pix_fmt", "bgra"])
            .args(["-s", &format!("{}x{}", cfg.width, cfg.height)])
            .args(["-r", &cfg.fps.to_string()])
            .args(["-i", "pipe:0"])
            // Audio input: raw s16le PCM on the child's stdout.
            .args(["-f", "s16le", "-ar", "48000", "-ac", "2", "-i", "pipe:1"])
            .args(["-c:v", cfg.encoder_name()])
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
            .stdout(Stdio::from(audio_reader))
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "spawning {} (requested encoder: {})",
                bin.ffmpeg.display(),
                cfg.encoder_name()
            )
        })?;

        // Video goes to the child's stdin; audio to the write end of the pipe whose
        // read end is now the child's stdout.
        let video_in = child.stdin.take().context("child stdin unavailable")?;

        let (video_tx, video_rx) = mpsc::channel::<Vec<u8>>();
        let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>();

        let video_writer = std::thread::Builder::new()
            .name("ffmpeg-video-in".into())
            .spawn(move || pump(video_rx, video_in))
            .context("spawning video writer thread")?;
        let audio_writer = std::thread::Builder::new()
            .name("ffmpeg-audio-in".into())
            .spawn(move || pump(audio_rx, audio_writer))
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

/// Copy queued chunks into a pipe until the sender half is dropped, then close it.
/// Returning ends the function, dropping `sink` and signalling EOF to ffmpeg.
fn pump<W: Write>(rx: mpsc::Receiver<Vec<u8>>, mut sink: W) -> std::io::Result<()> {
    for chunk in rx {
        sink.write_all(&chunk)?;
    }
    sink.flush()
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
        // Dropping the senders ends each `pump` loop, closing both pipes — which is
        // what tells ffmpeg the inputs have ended.
        self.video_tx.take();
        self.audio_tx.take();
        join_writer(self.video_writer.take(), "video")?;
        join_writer(self.audio_writer.take(), "audio")?;

        let status = self.child.wait().context("waiting for ffmpeg")?;
        if !status.success() {
            let mut stderr = String::new();
            if let Some(mut e) = self.child.stderr.take() {
                use std::io::Read;
                let _ = e.read_to_string(&mut stderr);
            }
            bail!(
                "ffmpeg exited with {status} using encoder '{}': {}",
                self.encoder_name,
                stderr.trim()
            );
        }
        Ok(())
    }

    fn active_encoder(&self) -> &'static str {
        self.encoder_name
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
