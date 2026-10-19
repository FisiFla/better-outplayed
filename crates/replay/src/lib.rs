//! localplay replay buffer: the in-memory ring, segment ledger, window resolution, clip splicing.
//!
//! [`MemoryRingBuffer`] keeps footage as fragments in RAM, so a buffer merely waiting for
//! something worth clipping writes nothing to disk, and a clip is spliced straight out of
//! memory with `-c copy`.
//!
//! The crate also holds pieces a session uses, so they are not private to the ring: `ledger` and
//! `scanner` index and evict any collection of segment files, `window` resolves a clip's range
//! against it, and `splice` is the lossless cut both paths share. `buffer` keeps only the two
//! values the recorder exchanges with a ring.

// Test bodies may unwrap and may chunk by a constant; production code may do neither.
// Scoped to the test build of this crate, so it cannot hide either in shipped code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks))]

pub mod buffer;
pub mod ledger;
pub mod ram_buffer;
pub mod scanner;
pub mod splice;
pub mod window;

pub use ledger::{Segment, SegmentLedger};
pub use ram_buffer::{MemoryRingBuffer, MemorySegment, MemoryStats, MemoryWindow};
