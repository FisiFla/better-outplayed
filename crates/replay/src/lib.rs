//! localplay replay buffer: segment ledger, window resolution, clip splicing.
//!
//! Two rings live here and they hold the same thing in different places. [`buffer::RingBuffer`]
//! keeps footage as MP4 files in a scratch directory — a file per second, deleted as they age —
//! which is what makes a crash survivable and is why session mode uses it. [`MemoryRingBuffer`]
//! keeps the same footage as fragments in **RAM**, so a replay buffer that is only *waiting* for
//! something worth clipping writes nothing to disk at all.

pub mod buffer;
pub mod ledger;
pub mod ram_buffer;
pub mod scanner;
pub mod splice;
pub mod window;

pub use ledger::{Segment, SegmentLedger};
pub use ram_buffer::{MemoryRingBuffer, MemorySegment, MemoryStats, MemoryWindow};
