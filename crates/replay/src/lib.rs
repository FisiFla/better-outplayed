//! localplay replay buffer: the in-memory ring, segment ledger, window resolution, clip splicing.
//!
//! [`MemoryRingBuffer`] keeps footage as fragments in **RAM**, so a replay buffer that is only
//! *waiting* for something worth clipping writes nothing to disk at all, and a clip is spliced
//! straight out of memory with `-c copy`.
//!
//! The module also holds the pieces a *session* uses, which is why they are not private to the
//! ring: `ledger` and `scanner` are how any collection of segment files is indexed and evicted,
//! `window` is how a clip's range is resolved against it, and `splice` is the lossless cut both
//! paths share. `buffer` keeps only the two values the recorder exchanges with a ring.

pub mod buffer;
pub mod ledger;
pub mod ram_buffer;
pub mod scanner;
pub mod splice;
pub mod window;

pub use ledger::{Segment, SegmentLedger};
pub use ram_buffer::{MemoryRingBuffer, MemorySegment, MemoryStats, MemoryWindow};
