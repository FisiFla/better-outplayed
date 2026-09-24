//! The two values the recorder exchanges with a ring, whatever the ring is made of.
//!
//! This module also held `RingBuffer` — a replay buffer backed by one MP4 file per second in a
//! scratch directory, deleted as they aged — and a `TriggerError` only its `trigger` returned.
//! Buffer mode keeps its footage in RAM now (`crate::ram_buffer`), which is the point of it, and
//! a session's files are the recorder's `SessionRing`, which shares this module's siblings
//! (`ledger`, `scanner`, `window`, `splice`) rather than the ring. Nothing reached the file-backed
//! ring any more, so it is gone rather than left as a second implementation nothing selects:
//! unreachable code that still compiles and still passes its own tests is the kind that rots in
//! place. `git log` has it if a crash-survivable *buffer* — as opposed to a crash-survivable
//! session, which session mode already is — is ever wanted back.
//!
//! What is left is shared by both rings: [`BufferConfig`] is what the recorder tells a ring to
//! build itself with, and [`BufferStats`] is what it asks one for.

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
