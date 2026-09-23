//! localplay replay buffer: segment ledger, window resolution, clip splicing.

pub mod ledger;
pub mod scanner;

pub use ledger::{Segment, SegmentLedger};
