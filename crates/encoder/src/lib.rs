//! Hardware video encoding through one long-lived ffmpeg child process.

use localplay_capture::{AudioBuffer, Frame};
use std::path::PathBuf;

pub mod ffmpeg;
pub mod probe;

pub use ffmpeg::FfmpegEncoder;
pub use probe::select_vendor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
}

impl VideoCodec {
    pub fn hw_encoder_name(self, vendor: Vendor) -> &'static str {
        match (self, vendor) {
            (VideoCodec::H264, Vendor::Nvenc) => "h264_nvenc",
            (VideoCodec::H264, Vendor::Qsv) => "h264_qsv",
            (VideoCodec::H264, Vendor::Amf) => "h264_amf",
            (VideoCodec::Hevc, Vendor::Nvenc) => "hevc_nvenc",
            (VideoCodec::Hevc, Vendor::Qsv) => "hevc_qsv",
            (VideoCodec::Hevc, Vendor::Amf) => "hevc_amf",
        }
    }
}

/// GPU encoder vendors. There is deliberately no software variant: a silent CPU
/// fallback would violate principle 3 (spec §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Nvenc,
    Qsv,
    Amf,
}

#[derive(Debug, Clone)]
pub struct EncodeConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub segment_ms: u64,
    pub scratch_dir: PathBuf,
    pub audio_bitrate_kbps: u32,
    /// `None` means "real hardware encoder" — the only shipping configuration.
    pub(crate) software_encoder: Option<&'static str>,
}

impl EncodeConfig {
    pub fn hardware(
        codec: VideoCodec,
        vendor: Vendor,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        segment_ms: u64,
        scratch_dir: PathBuf,
    ) -> Self {
        Self {
            codec,
            width,
            height,
            fps,
            bitrate_kbps,
            segment_ms,
            scratch_dir,
            audio_bitrate_kbps: 192,
            software_encoder: Some(codec.hw_encoder_name(vendor)),
        }
    }

    /// Only available to tests and dev builds. The CLI cannot construct this from
    /// a user config file, which is what keeps "no CPU fallback" honest.
    #[cfg(any(test, feature = "test-encoders"))]
    pub fn for_tests_software(
        codec: VideoCodec,
        width: u32,
        height: u32,
        fps: u32,
        scratch_dir: PathBuf,
        segment_ms: u64,
    ) -> Self {
        Self {
            codec,
            width,
            height,
            fps,
            bitrate_kbps: 2_000,
            segment_ms,
            scratch_dir,
            audio_bitrate_kbps: 128,
            software_encoder: None,
        }
    }

    pub(crate) fn encoder_name(&self) -> &'static str {
        self.software_encoder.unwrap_or("libx264")
    }
}

pub trait Encoder: Send {
    fn submit_video(&mut self, frame: &Frame) -> anyhow::Result<()>;
    fn submit_audio(&mut self, audio: &AudioBuffer) -> anyhow::Result<()>;
    fn finish(&mut self) -> anyhow::Result<()>;
    /// Codec actually in use, for ffprobe assertions and the UI.
    fn active_encoder(&self) -> &'static str;
}
