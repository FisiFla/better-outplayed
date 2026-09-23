//! localplay media drivers: locating ffmpeg and performing lossless edits.

pub mod binaries;
pub mod probe;

pub use binaries::FfmpegBinaries;
pub use probe::MediaInfo;
