//! localplay media drivers: locating ffmpeg and performing lossless edits.

pub mod binaries;
pub mod edit;
pub mod probe;

pub use binaries::{wait_with_deadline, FfmpegBinaries};
pub use probe::{ffmpeg_reason, smoke_test_encoder, AvDrift, MediaInfo};
