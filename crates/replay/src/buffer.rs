//! The two values the recorder exchanges with a ring, whatever the ring is made of:
//! [`BufferConfig`] is what the recorder builds a ring with, and [`BufferStats`] is what it asks
//! one for. Buffer mode keeps its footage in RAM (`crate::ram_buffer`); a session's files are
//! the recorder's `SessionRing`, which shares `ledger`, `scanner`, `window` and `splice`.

use std::path::PathBuf;

/// What the recorder tells a ring to build itself with.
#[derive(Debug, Clone)]
pub struct BufferConfig {
    pub pre_ms: u64,
    pub post_ms: u64,
    pub segment_ms: u64,
    pub clips_dir: PathBuf,
}

/// What the recorder asks a ring for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferStats {
    pub segments: usize,
    pub bytes_on_disk: u64,
    pub span_ms: u64,
}
