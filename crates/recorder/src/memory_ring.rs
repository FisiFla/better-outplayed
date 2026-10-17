//! The in-memory replay ring and the thread that feeds it.
//!
//! `localplay_replay::MemoryRingBuffer` holds the footage (it takes bytes, so its rules are testable
//! without a pipe); this owns the reader thread on the encoder's output pipe and the lock that
//! lets the pump query the span while the reader adds to it.
//!
//! # A silent reader is the failure this module is built around
//!
//! If the reader stops, nothing raises an error: ffmpeg keeps encoding into a pipe that absorbs
//! several seconds, the pump keeps submitting, and the only symptom is short clips or a frozen
//! `span_ms`. So every exit from the read loop records why it stopped, a panic included (the loop
//! is in `catch_unwind` because a panic bypasses the `?` error path), and [`MemoryRing::trigger`]
//! refuses to serve a clip from a ring known to be dead.
//!
//! # Lock discipline
//!
//! The reader holds the lock while pushing one fragment. A clip assembles its bytes under the lock
//! and splices outside it: the splice runs ffmpeg for as long as the muxer takes, and holding the
//! lock would block the reader, fill the pipe, stall ffmpeg's stdin and the recording still running.

use anyhow::{bail, Context, Result};
use localplay_media::FfmpegBinaries;
use localplay_replay::splice::ClipSplicer;
use localplay_replay::{MemoryRingBuffer, MemoryStats};
use std::io::Read;
use std::path::PathBuf;
use std::process::ChildStdout;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

/// How much is read from the pipe at a time: several fragments on a busy stream, bounding the thread's allocation to 256KiB.
const READ_CHUNK: usize = 256 * 1024;

/// Everything a ring needs that is not the stream itself.
///
/// A struct because five of the fields are bare numbers: a positional `pre_ms`/`cap_ms` swap would
/// silently hold the wrong footage.
pub struct RingSetup {
    pub bin: FfmpegBinaries,
    pub clips_dir: PathBuf,
    /// Bytes of RAM the ring may hold before it evicts.
    pub ram_cap_bytes: u64,
    /// Media time the ring may hold before it evicts — the window a trigger can ask for.
    pub cap_ms: u64,
    /// Media time kept before a trigger.
    pub pre_ms: u64,
    /// Media time kept after a trigger.
    pub post_ms: u64,
    /// The encoder that actually ran, for the clip's metadata.
    pub encoder: String,
}

pub struct MemoryRing {
    shared: Arc<Mutex<MemoryRingBuffer>>,
    /// Why the reader stopped before the stream ended, if it did. Without it a dead ring answers every
    /// clip request with "nothing covers that range", which reads as a short buffer.
    failure: Arc<Mutex<Option<String>>>,
    reader: Option<JoinHandle<()>>,
    bin: FfmpegBinaries,
    clips_dir: PathBuf,
    pre_ms: u64,
    post_ms: u64,
    encoder: String,
}

impl MemoryRing {
    /// Start reading `stream` into a ring of at most `ram_cap_bytes` / `cap_ms`.
    ///
    /// `stream` is the encoder's stdout (it exists only with `EncodeOutput::FragmentedStream`), so this
    /// ring is its one reader. Creates the clips directory here so a permissions or path problem
    /// surfaces at recording start, not when the user presses the key.
    pub fn start(stream: ChildStdout, setup: RingSetup) -> Result<Self> {
        let RingSetup {
            bin,
            clips_dir,
            ram_cap_bytes,
            cap_ms,
            pre_ms,
            post_ms,
            encoder,
        } = setup;
        std::fs::create_dir_all(&clips_dir)
            .with_context(|| format!("creating the clips directory {}", clips_dir.display()))?;

        let shared = Arc::new(Mutex::new(MemoryRingBuffer::new(ram_cap_bytes, cap_ms)));
        let failure = Arc::new(Mutex::new(None));
        let ring = Arc::clone(&shared);
        let slot = Arc::clone(&failure);
        let reader = std::thread::Builder::new()
            .name("localplay-ram-ring".into())
            .spawn(move || {
                // A panic would otherwise kill this thread silently and show up only as short clips later; catch, name and record it.
                let hook = Arc::clone(&slot);
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    read_until_eof(stream, ring, Arc::clone(&hook))
                }));
                if let Err(panic) = outcome {
                    record_failure(
                        &slot,
                        format!("the reader panicked: {}", describe_panic(&panic)),
                    );
                }
            })
            .expect("spawning the in-memory ring's reader thread");
        Ok(Self {
            shared,
            failure,
            reader: Some(reader),
            bin,
            clips_dir,
            pre_ms,
            post_ms,
            encoder,
        })
    }

    /// The ring's lock, or the reason it cannot be taken.
    ///
    /// Never folds a failure into zero: a poisoned lock (the reader died) and a live but empty ring look
    /// identical as a number, and only the first is a bug.
    fn lock(&self) -> Result<MutexGuard<'_, MemoryRingBuffer>> {
        self.shared.lock().map_err(|_| {
            anyhow::anyhow!(
                "the in-memory ring's lock is poisoned, so the reader thread died holding it{}",
                self.failure
                    .lock()
                    .ok()
                    .and_then(|f| f.clone())
                    .map(|why| format!(": {why}"))
                    .unwrap_or_default()
            )
        })
    }

    /// Media time the ring can prove it holds, run-relative. Zero only when genuinely empty; an
    /// unreadable ring is an error.
    pub fn span_ms(&self) -> Result<u64> {
        Ok(self.lock()?.span_ms())
    }

    /// What the ring holds, for the status line.
    pub fn stats(&self) -> Result<MemoryStats> {
        Ok(self.lock()?.stats())
    }

    /// Bytes held in RAM.
    pub fn bytes(&self) -> Result<u64> {
        Ok(self.stats()?.ram_bytes)
    }

    /// A memory ring starts empty every run (nothing to adopt, no segment number to continue); the
    /// accessor exists so the engine can treat both rings alike.
    pub fn origin_ms(&self) -> u64 {
        0
    }

    /// Wait for the reader to finish.
    ///
    /// Called after the encoder is flushed and its child has exited, which closes the pipe, so this
    /// returns promptly. Returns whether the reader ended cleanly, so a shutdown can report a dead ring.
    pub fn join(&mut self) -> Result<()> {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        match self.failure.lock().expect("the failure slot").clone() {
            Some(why) => bail!("the in-memory ring's reader stopped early: {why}"),
            None => Ok(()),
        }
    }

    /// Save a clip around `trigger_ms` out of the footage in RAM: window `[trigger - pre, trigger + post)`,
    /// written as `{stem}.mp4` in the clips directory, like the other ring.
    pub fn trigger(
        &self,
        trigger_ms: u64,
        stem: &str,
    ) -> Result<localplay_replay::splice::ClipMetadata> {
        self.trigger_window(trigger_ms, stem, self.pre_ms, self.post_ms)
    }

    pub fn bin(&self) -> &FfmpegBinaries {
        &self.bin
    }

    /// [`MemoryRing::trigger`] with this clip's own pre- and post-roll, already clamped to what the ring can hold.
    pub fn trigger_window(
        &self,
        trigger_ms: u64,
        stem: &str,
        pre_ms: u64,
        post_ms: u64,
    ) -> Result<localplay_replay::splice::ClipMetadata> {
        if let Some(why) = self.failure.lock().expect("the failure slot").clone() {
            bail!(
                "the in-memory ring stopped reading the encoder's stream, so there is no footage \
                 to clip: {why}"
            );
        }
        let start = trigger_ms.saturating_sub(pre_ms);
        let end = trigger_ms.saturating_add(post_ms);

        // Assemble under the lock, splice outside it (see the module note).
        let (bytes, audio_tracks, window_ms, held_ms, oldest_ms, newest_ms) = {
            let ring = self.lock()?;
            let held_ms = ring.span_ms();
            let oldest_ms = ring.segments().next().map(|s| s.start_ms).unwrap_or(0);
            let newest_ms = ring.segments().last().map(|s| s.end_ms).unwrap_or(0);
            let window = ring.window(start, end).with_context(|| {
                format!(
                    "nothing in the ring covers [{start}ms, {end}ms): it holds {held_ms}ms of \
                     footage from {oldest_ms}ms to {newest_ms}ms"
                )
            })?;
            window.validate()?;
            if window.truncated_front {
                tracing::warn!(
                    "only {}ms of pre-roll was buffered for a {}ms request",
                    window.duration_ms(),
                    pre_ms
                );
            }
            let window_ms = window.duration_ms();
            (
                window.assemble(),
                window.audio_tracks(),
                window_ms,
                held_ms,
                oldest_ms,
                newest_ms,
            )
        };

        // What was asked for, what the ring held and what the window spans: what a short clip is diagnosed from.
        tracing::info!(
            "clip from RAM: trigger={trigger_ms}ms window=[{start}ms, {end}ms) selected={window_ms}ms \
             held={held_ms}ms footage=[{oldest_ms}ms, {newest_ms}ms] bytes={}",
            bytes.len()
        );

        let out = self.clips_dir.join(format!("{stem}.mp4"));
        ClipSplicer::splice_stream(&self.bin, bytes, &out, &self.encoder, audio_tracks)
    }
}

impl crate::pump::MediaRing for MemoryRing {
    /// Nothing to scan: this ring is fed by its own reader thread, which has pushed every fragment the
    /// encoder wrote, and a fragment is only pushed once its `mdat` arrived in full.
    fn scan(&mut self) -> Result<()> {
        Ok(())
    }

    /// Media time this ring can prove it holds — what the post-roll wait advances against.
    fn span_ms(&self) -> u64 {
        // The wait's call cannot return a `Result`. An unreadable ring reports 0, a span no post-roll
        // target is satisfied by, so the wait times out loudly rather than splicing footage that is not
        // there. The cause is already logged once by `record_failure`.
        MemoryRing::span_ms(self).unwrap_or(0)
    }
}

/// Read the encoder's stream into the ring until the stream ends.
fn read_until_eof(
    mut stream: ChildStdout,
    ring: Arc<Mutex<MemoryRingBuffer>>,
    failure: Arc<Mutex<Option<String>>>,
) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let read = match stream.read(&mut buf) {
            Ok(0) => return, // the encoder closed the pipe: a clean end of stream
            Ok(n) => n,
            // A signal interrupted the read; the stream is still open.
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                // The child died or the pipe broke. Not worth failing the recording over, but the buffer is dead and a clip request must say so.
                record_failure(&failure, format!("reading the encoder's stream failed: {err}"));
                return;
            }
        };
        let Ok(mut guard) = ring.lock() else {
            record_failure(&failure, "the ring's lock was poisoned".to_string());
            return;
        };
        if let Err(err) = guard.push(&buf[..read]) {
            record_failure(
                &failure,
                format!("the encoder's stream could not be parsed: {err:#}"),
            );
            return;
        }
    }
}

/// Record and log the first reason the reader stopped, and stay quiet about later ones.
fn record_failure(slot: &Arc<Mutex<Option<String>>>, why: String) {
    let mut guard = slot.lock().expect("the failure slot");
    if guard.is_none() {
        tracing::error!(
            "the in-memory replay ring has stopped: {why}. Recording continues, but no clip can \
             be taken from the buffer until it is restarted"
        );
        *guard = Some(why);
    }
}

/// A panic payload as text, for a log line and a returned error.
fn describe_panic(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic with no message".to_string()
    }
}
