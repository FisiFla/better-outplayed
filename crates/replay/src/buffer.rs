//! The replay ring: scratch directory, ledger, eviction, and clip extraction.

use crate::ledger::{Segment, SegmentLedger};
use crate::scanner::{self, newly_complete, segment_seq};
use crate::splice::{ClipMetadata, ClipSplicer};
use crate::window::{self, WindowError};
use anyhow::{Context, Result};
use localplay_media::FfmpegBinaries;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct BufferConfig {
    pub pre_ms: u64,
    pub post_ms: u64,
    pub scratch_cap_bytes: u64,
    pub segment_ms: u64,
    pub clips_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferStats {
    pub segments: usize,
    pub bytes_on_disk: u64,
    pub span_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum TriggerError {
    #[error(transparent)]
    Window(#[from] WindowError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub struct RingBuffer {
    cfg: BufferConfig,
    bin: FfmpegBinaries,
    scratch_dir: PathBuf,
    ledger: SegmentLedger,
    highest_known: Option<u64>,
    /// Offset, in ms, of this run's first segment on the ledger's timeline:
    /// `reserve_segment_number() * segment_ms`.
    ///
    /// A segment's `start_ms` is `seq * segment_ms` — the ledger's timeline is the
    /// *ring's*, not any one run's, because that is what makes an adopted segment's
    /// position meaningful. This run's own timeline (what `stats().span_ms` and the
    /// trigger's `trigger_ms` are measured on, both being counts of captured seconds
    /// since this process started) begins at that offset.
    ///
    /// Zero until [`RingBuffer::reserve_segment_number`] is called, which is exactly
    /// right for a run that starts on an empty scratch directory — and for the tests,
    /// which build a ledger and a timeline out of the same numbers.
    origin_ms: u64,
    /// Name of the encoder actually in use, recorded on every clip so an ffprobe
    /// mismatch is detectable (spec §11 criterion 5).
    encoder: String,
}

impl RingBuffer {
    pub fn start(
        bin: &FfmpegBinaries,
        cfg: BufferConfig,
        scratch_dir: PathBuf,
        encoder: String,
    ) -> Result<Self> {
        std::fs::create_dir_all(&scratch_dir)
            .with_context(|| format!("creating {}", scratch_dir.display()))?;
        std::fs::create_dir_all(&cfg.clips_dir)
            .with_context(|| format!("creating {}", cfg.clips_dir.display()))?;
        Ok(Self {
            cfg,
            bin: bin.clone(),
            scratch_dir,
            ledger: SegmentLedger::default(),
            highest_known: None,
            origin_ms: 0,
            encoder,
        })
    }

    /// Adopt segments already on disk from a previous run.
    pub fn adopt_existing(&mut self) -> Result<usize> {
        let before = self.ledger.len();
        self.scan_once()?;
        Ok(self.ledger.len() - before)
    }

    /// Reserve the sequence number this run's encoder must start writing at, and adopt
    /// it as the origin of this run's timeline.
    ///
    /// Must be called after [`RingBuffer::adopt_existing`] and **before** the encoder is
    /// spawned, because the number is an ffmpeg argument (`-segment_start_number`):
    /// ffmpeg's segment muxer restarts its numbering at 0 every time it is spawned, so a
    /// second run would otherwise write over the files the adopted ledger points at —
    /// the ledger would name footage the new run had replaced, and a clip cut from it
    /// would contain the wrong picture. The number itself comes from
    /// [`scanner::next_segment_number`], which takes the maximum of the ledger's seqs
    /// and the filenames on disk; the disk matters as well as the ledger because a crash
    /// can leave a segment written but not yet indexed.
    pub fn reserve_segment_number(&mut self) -> Result<u64> {
        let names = self.observed_names()?;
        let number = scanner::next_segment_number(&self.ledger.seqs(), &names);
        self.origin_ms = number.saturating_mul(self.cfg.segment_ms);
        Ok(number)
    }

    /// Find newly completed segments, index them, and evict to the cap.
    pub fn scan_once(&mut self) -> Result<()> {
        let observed = self.observed_seqs()?;
        for seq in newly_complete(&observed, self.highest_known) {
            let file = self.segment_path(seq);
            let bytes = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            self.ledger.push(Segment {
                seq,
                file,
                start_ms: seq * self.cfg.segment_ms,
                duration_ms: self.cfg.segment_ms,
                bytes,
            });
            self.highest_known = Some(seq);
        }
        self.enforce_cap()?;
        Ok(())
    }

    /// Delete oldest segments until under the cap. Deleting in Rust rather than
    /// using `-segment_wrap` keeps the ledger authoritative.
    fn enforce_cap(&mut self) -> Result<()> {
        for seg in self.ledger.evict_to_cap(self.cfg.scratch_cap_bytes) {
            if let Err(e) = std::fs::remove_file(&seg.file) {
                tracing::warn!("could not evict {}: {e}", seg.file.display());
            }
        }
        Ok(())
    }

    fn observed_seqs(&self) -> Result<Vec<u64>> {
        Ok(self.observed_names()?.iter().filter_map(|n| segment_seq(n)).collect())
    }

    /// Every filename in the scratch directory, unfiltered — the caller decides what it
    /// recognises (segments for the ledger, `seg-%06d.mp4` for the numbering).
    fn observed_names(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.scratch_dir)
            .with_context(|| format!("reading {}", self.scratch_dir.display()))?
        {
            names.push(entry?.file_name().to_string_lossy().into_owned());
        }
        Ok(names)
    }

    fn segment_path(&self, seq: u64) -> PathBuf {
        self.scratch_dir.join(format!("seg-{seq:06}.mp4"))
    }

    pub fn stats(&self) -> BufferStats {
        BufferStats {
            segments: self.ledger.len(),
            bytes_on_disk: self.ledger.total_bytes(),
            // Measured from the start of *this run*, not from the ledger's origin: a
            // segment number is no longer the same as the second it was captured in
            // (see `origin_ms`).
            span_ms: self.ledger.span_ms().saturating_sub(self.origin_ms),
        }
    }

    /// The ledger's own position, in ms, of this run's zero.
    ///
    /// `stats().span_ms` and `trigger`'s `trigger_ms` are run-relative media time; adding
    /// this turns either into a position on the ledger's timeline, which is the timeline
    /// segment numbers (and so a clip's footage) are actually measured on. A caller that
    /// persists a position — the clip index does, as `clips.started_at` — wants the
    /// ledger's, so that clips spliced in successive runs against the same scratch
    /// directory keep their order.
    pub fn ledger_origin_ms(&self) -> u64 {
        self.origin_ms
    }

    /// Persist the ledger so the index survives a crash (spec §6.4).
    pub fn save_ledger(&self) -> Result<()> {
        self.ledger.save_atomic(&self.scratch_dir.join("ledger.toml"))
    }

    /// Extract a clip around `trigger_ms` on the capture timeline.
    ///
    /// Caller must ensure the post-roll has been written; `resolve` returns
    /// `PostRollUnavailable` otherwise.
    pub fn trigger(&self, trigger_ms: u64, stem: &str) -> Result<ClipMetadata, TriggerError> {
        let win = window::resolve(
            &self.ledger,
            // `trigger_ms` is seconds-since-process-start, i.e. run-relative; the
            // ledger's timeline is absolute (see `origin_ms`).
            self.origin_ms + trigger_ms,
            self.cfg.pre_ms,
            self.cfg.post_ms,
        )?;
        if win.truncated_front {
            tracing::warn!(
                "only {}ms of pre-roll was buffered for a {}ms request",
                win.segments.first().map(|_| win.duration_ms()).unwrap_or(0),
                self.cfg.pre_ms
            );
        }
        let out = self.cfg.clips_dir.join(format!("{stem}.mp4"));
        let meta = ClipSplicer::splice(&self.bin, &win, &out, &self.encoder)?;
        Ok(meta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A placeholder pair: everything tested here (`start`, `adopt_existing`,
    /// `reserve_segment_number`, `scan_once`, `stats`) only reads and writes the scratch
    /// directory, so no ffmpeg process is involved and this stays hermetic.
    fn bin() -> FfmpegBinaries {
        FfmpegBinaries { ffmpeg: "ffmpeg".into(), ffprobe: "ffprobe".into() }
    }

    fn cfg(clips_dir: &Path) -> BufferConfig {
        BufferConfig {
            pre_ms: 3_000,
            post_ms: 1_000,
            scratch_cap_bytes: 1 << 30,
            segment_ms: 1_000,
            clips_dir: clips_dir.to_path_buf(),
        }
    }

    /// A file named as ffmpeg names segments. Its contents are irrelevant to the ledger.
    fn touch(dir: &Path, seq: u64) {
        std::fs::write(dir.join(format!("seg-{seq:06}.mp4")), b"not really a segment")
            .expect("write a scratch segment file");
    }

    fn ring(scratch: &Path, clips: &Path) -> RingBuffer {
        RingBuffer::start(&bin(), cfg(clips), scratch.to_path_buf(), "libx264".into())
            .expect("start the ring buffer")
    }

    #[test]
    fn continuing_the_numbering_keeps_this_runs_timeline_starting_at_zero() {
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let clips = tempfile::tempdir().expect("a clips dir");
        // What a previous run leaves behind: files 0..=4 on disk. `adopt_existing` indexes
        // 0..=3 — a segment is only trusted once a strictly later one exists — so the
        // ledger's span is 4000ms before this run has captured anything.
        for seq in 0..=4 {
            touch(scratch.path(), seq);
        }

        let mut ring = ring(scratch.path(), clips.path());
        assert_eq!(ring.adopt_existing().expect("adopt"), 4);

        // The encoder must continue from 5, not restart at 0 (Fix: overwriting).
        assert_eq!(ring.reserve_segment_number().expect("reserve"), 5);

        // And the adopted footage must NOT read as this run's captured timeline. The
        // ledger's span is still 4000ms of *ring* timeline, but this process has captured
        // nothing: reporting 4000ms here would make every post-roll wait return instantly
        // and splice the previous run's footage into the clip.
        assert_eq!(
            ring.stats().span_ms,
            0,
            "run-relative span must start at zero, not at the adopted material's position"
        );

        // The first segments of this run (5, then 6 so that 5 counts as complete) put a
        // second of footage at the start of this run's timeline, not at 5-6s of it.
        touch(scratch.path(), 5);
        touch(scratch.path(), 6);
        ring.scan_once().expect("scan");
        assert_eq!(
            ring.stats().span_ms,
            1_000,
            "one second of this run's footage is one second, whatever number it carries"
        );
    }

    #[test]
    fn a_first_run_on_an_empty_scratch_directory_still_numbers_from_zero() {
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let clips = tempfile::tempdir().expect("a clips dir");
        let mut ring = ring(scratch.path(), clips.path());

        assert_eq!(ring.adopt_existing().expect("adopt"), 0);
        assert_eq!(ring.reserve_segment_number().expect("reserve"), 0);
        assert_eq!(ring.stats().span_ms, 0);
    }
}
