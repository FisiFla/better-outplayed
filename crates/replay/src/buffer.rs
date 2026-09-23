//! The replay ring: scratch directory, ledger, eviction, and clip extraction.

use crate::ledger::{Segment, SegmentLedger};
use crate::scanner::newly_complete;
use crate::splice::{ClipMetadata, ClipSplicer};
use crate::window::{self, WindowError};
use anyhow::{Context, Result};
use localplay_media::FfmpegBinaries;
use std::path::{Path, PathBuf};

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
            encoder,
        })
    }

    /// Adopt segments already on disk from a previous run.
    pub fn adopt_existing(&mut self) -> Result<usize> {
        let before = self.ledger.len();
        self.scan_once()?;
        Ok(self.ledger.len() - before)
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
        let mut seqs = Vec::new();
        for entry in std::fs::read_dir(&self.scratch_dir)
            .with_context(|| format!("reading {}", self.scratch_dir.display()))?
        {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix("seg-") {
                if let Some(num) = rest.strip_suffix(".mp4") {
                    if let Ok(seq) = num.parse::<u64>() {
                        seqs.push(seq);
                    }
                }
            }
        }
        Ok(seqs)
    }

    fn segment_path(&self, seq: u64) -> PathBuf {
        self.scratch_dir.join(format!("seg-{seq:06}.mp4"))
    }

    pub fn stats(&self) -> BufferStats {
        BufferStats {
            segments: self.ledger.len(),
            bytes_on_disk: self.ledger.total_bytes(),
            span_ms: self.ledger.span_ms(),
        }
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
            trigger_ms,
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
