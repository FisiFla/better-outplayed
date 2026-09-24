//! The in-memory replay ring, and the thread that feeds it.
//!
//! `localplay_replay::MemoryRingBuffer` holds the footage; this owns the two things a ring
//! cannot own by itself — the **reader thread** on the encoder's output pipe, and the lock that
//! lets the pump ask how much footage there is while the reader is adding more.
//!
//! # Why the reading is here and not in the ring
//!
//! The ring takes bytes (`MemoryRingBuffer::push`) rather than a stream, which is what lets all
//! of its rules be tested without a pipe, a child process or a thread. A pipe has exactly one
//! reader and a lifetime, so the reading belongs with whoever owns the encoder — here.
//!
//! # A silent reader is the failure this module is built around
//!
//! If the reader stops, nothing raises an error anywhere: ffmpeg keeps encoding happily into a
//! pipe that a 64KiB buffer absorbs for several seconds of small frames, the pump keeps
//! submitting frames at its configured rate, and the only symptom is that clips come back short
//! — or that `span_ms` stops moving. Every exit from the read loop therefore records *why* it
//! stopped, a panic included (a panic unwinds past the `?`-based error path entirely, which is
//! why the loop is wrapped in `catch_unwind`), and [`MemoryRing::trigger`] refuses to serve a
//! clip from a ring it knows is dead rather than reporting footage that is not there.
//!
//! # The lock discipline, which is not optional
//!
//! The reader holds the lock while pushing one fragment. A clip assembles its bytes **under**
//! the lock and splices **outside** it, because the splice runs ffmpeg for as long as the muxer
//! takes: holding the lock there would block the reader, which fills the pipe, which stops
//! ffmpeg draining its stdin, which stalls the recording that is still running. The window type
//! is borrowed from the ring for the same reason — it cannot outlive the lock, so the bytes are
//! lifted out before it drops.

use anyhow::{bail, Context, Result};
use localplay_media::FfmpegBinaries;
use localplay_replay::splice::ClipSplicer;
use localplay_replay::{MemoryRingBuffer, MemoryStats};
use std::io::Read;
use std::path::PathBuf;
use std::process::ChildStdout;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

/// How much is read from the pipe at a time.
///
/// A fragment is tens to hundreds of kilobytes, so this reads several at once on a busy stream
/// and still bounds the thread's own allocation to 256KiB.
const READ_CHUNK: usize = 256 * 1024;

/// Everything a ring needs that is not the stream itself.
///
/// A struct rather than six more parameters because five of them are bare numbers that have to be
/// right: a positional `pre_ms` and `cap_ms` swapped is a buffer that holds the wrong footage, and
/// nothing about a call site of integers says which is which.
pub struct RingSetup {
    pub bin: FfmpegBinaries,
    /// Where a saved clip goes.
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
    /// Why the reader stopped before the stream ended, if it did.
    ///
    /// A ring that silently stopped reading answers every clip request with "nothing covers that
    /// range", which reads as a buffer that is too short rather than a buffer that is dead.
    /// `trigger` refuses with this instead.
    failure: Arc<Mutex<Option<String>>>,
    reader: Option<JoinHandle<()>>,
    bin: FfmpegBinaries,
    /// Where a saved clip goes.
    clips_dir: PathBuf,
    pre_ms: u64,
    post_ms: u64,
    encoder: String,
}

impl MemoryRing {
    /// Start reading `stream` into a ring of at most `ram_cap_bytes` / `cap_ms`.
    ///
    /// `stream` is the encoder's stdout, which only exists because it was spawned with
    /// `EncodeOutput::FragmentedStream`; taking it here is what makes this ring the stream's one
    /// reader.
    ///
    /// Creates the clips directory, as `RingBuffer::start` does and for the same reason: doing
    /// it here means a permissions or path problem is reported when the recording starts rather
    /// than at the moment the user presses the key and is waiting for a clip.
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
                // A panic would otherwise kill this thread silently, and the only symptom would
                // be short clips some seconds later. Catch it, name it, and record it.
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
    /// Never swallows a failure into a zero: a poisoned lock and a live-but-empty ring look
    /// identical to a caller that only sees a number, and the first is a bug while the second is
    /// a fact. The reader poisoning it is the case that matters — see the module note.
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

    /// Media time the ring can prove it holds, run-relative.
    ///
    /// Zero only when the ring is genuinely empty; a ring that cannot be read is an error, not a
    /// zero.
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

    /// The memory ring starts empty every run: there is nothing on disk to adopt, and no segment
    /// number to continue. Both accessors exist so the engine can treat the two rings the same
    /// way without pretending they are alike.
    pub fn origin_ms(&self) -> u64 {
        0
    }

    /// Wait for the reader to finish.
    ///
    /// Called after the encoder has been flushed and its child has exited, which is what closes
    /// the pipe and ends the stream — so this returns rather than blocking. Joining is worth
    /// doing: the thread holds the last fragment's bytes, and a ring still being written to while
    /// the session is closed over it is a race with no purpose.
    ///
    /// Returns whether the reader ended cleanly, so a shutdown can report a ring that died
    /// rather than pretending the footage is all there.
    pub fn join(&mut self) -> Result<()> {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        match self.failure.lock().expect("the failure slot").clone() {
            Some(why) => bail!("the in-memory ring's reader stopped early: {why}"),
            None => Ok(()),
        }
    }

    /// Save a clip around `trigger_ms` out of the footage in RAM.
    ///
    /// The same window arithmetic as `RingBuffer::trigger` — `[trigger - pre, trigger + post)` —
    /// and the same destination (`{stem}.mp4` in the clips directory), because the caller should
    /// not have to know which kind of ring it is talking to.
    pub fn trigger(
        &self,
        trigger_ms: u64,
        stem: &str,
    ) -> Result<localplay_replay::splice::ClipMetadata> {
        if let Some(why) = self.failure.lock().expect("the failure slot").clone() {
            bail!(
                "the in-memory ring stopped reading the encoder's stream, so there is no footage \
                 to clip: {why}"
            );
        }
        let start = trigger_ms.saturating_sub(self.pre_ms);
        let end = trigger_ms.saturating_add(self.post_ms);

        // Assemble under the lock, splice outside it. See the module note.
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
                    self.pre_ms
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

        // The numbers a short clip is diagnosed from: what was asked for, what the ring held at
        // that instant, and what the selected window actually spans. A pass has already been
        // spent inferring this from surrounding test output; it is cheap to just say it.
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
    /// Nothing to scan.
    ///
    /// The file ring's `scan` exists because its segments appear on disk behind the encoder's
    /// back; this ring is fed by its own reader thread, which has already pushed every fragment
    /// the encoder wrote. It also cannot have the file ring's problem of a segment still being
    /// appended to — a fragment is only pushed once its `mdat` has arrived in full.
    fn scan(&mut self) -> Result<()> {
        Ok(())
    }

    /// Media time this ring can prove it holds — what the post-roll wait advances against.
    fn span_ms(&self) -> u64 {
        // The wait's own call, and it cannot return a `Result`.
        //
        // A ring that cannot be read reports 0 — a span no post-roll target is ever satisfied
        // by, so the wait ends in its own loud timeout instead of splicing a clip out of footage
        // that is not there. The reason itself is already in the log: a ring only becomes
        // unreadable by the reader failing, and `record_failure` logs that once, with the cause.
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
                // The child died, or the pipe broke. Not an error worth failing the recording
                // over — but the buffer is now dead, and a clip request must say so rather than
                // claim the footage never existed.
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
