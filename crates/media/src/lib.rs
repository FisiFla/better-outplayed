//! localplay media drivers: locating ffmpeg and performing lossless edits.

pub use binaries::sidecar_command;
pub mod binaries;
pub mod edit;
pub mod fragments;
pub mod probe;

pub use binaries::{wait_with_deadline, FfmpegBinaries};
pub use fragments::{Fragment, FragmentSplitter};
pub use probe::{
    ffmpeg_reason, smoke_test_encoder, stream_layout, AvDrift, MediaInfo, StreamLayout,
    StreamShape,
};
