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
    /// The frame size that arrives on the rawvideo pipe — the capture backend's
    /// native size. ffmpeg's `-s` is set from this, because it describes the
    /// incoming stream; if it is wrong the pipe desyncs.
    pub source_size: (u32, u32),
    /// The size of the encoded output. When this differs from `source_size` the
    /// encoder inserts a `scale` filter; when they are equal it adds none.
    pub output_size: (u32, u32),
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub segment_ms: u64,
    pub scratch_dir: PathBuf,
    pub audio_bitrate_kbps: u32,
    /// Sequence number of the first segment this encoder writes (`seg-%06d.mp4`).
    ///
    /// ffmpeg's segment muxer numbers files from 0 *every time it is spawned*, so a
    /// second run would write `seg-000000.mp4` straight over the file the adopted
    /// ledger still points at — the ledger then names footage the new run has replaced.
    /// The caller computes the first free number from what is already on disk
    /// (`RingBuffer::reserve_segment_number`) and passes it here.
    ///
    /// Fields are threaded through this struct rather than added as an argument to
    /// [`crate::FfmpegEncoder::spawn`] so that every caller of `spawn` (tests included)
    /// keeps compiling unchanged, and because this struct is already the one place that
    /// holds the ffmpeg arguments. `hardware` has no room for a ninth parameter without
    /// another clippy `too_many_arguments` warning. Default `0`: a first run on an empty
    /// scratch directory, and every test.
    pub start_number: u64,
    /// `None` means "real hardware encoder" — the only shipping configuration.
    pub(crate) software_encoder: Option<&'static str>,
}

impl EncodeConfig {
    pub fn hardware(
        codec: VideoCodec,
        vendor: Vendor,
        source_size: (u32, u32),
        output_size: (u32, u32),
        fps: u32,
        bitrate_kbps: u32,
        segment_ms: u64,
        scratch_dir: PathBuf,
    ) -> Self {
        Self {
            codec,
            source_size,
            output_size,
            fps,
            bitrate_kbps,
            segment_ms,
            scratch_dir,
            audio_bitrate_kbps: 192,
            // A first run (and every test) writes `seg-000000.mp4`; the CLI overrides
            // this from what it finds on disk before spawning.
            start_number: 0,
            software_encoder: Some(codec.hw_encoder_name(vendor)),
        }
    }

    /// Only available to tests and dev builds. The CLI cannot construct this from
    /// a user config file, which is what keeps "no CPU fallback" honest.
    ///
    /// Source and output are the same size: the stub sources feed the encoder at
    /// exactly the size it encodes, so no scaling is involved.
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
            source_size: (width, height),
            output_size: (width, height),
            fps,
            bitrate_kbps: 2_000,
            segment_ms,
            scratch_dir,
            audio_bitrate_kbps: 128,
            start_number: 0,
            software_encoder: None,
        }
    }

    /// The ffmpeg encoder this configuration selects — the same string
    /// [`Encoder::active_encoder`] reports once the child is running.
    ///
    /// Public because a caller has to be able to name the encoder *before* spawning it
    /// (the CLI records it on every clip), which is what lets the replay ring be built
    /// and the segment numbering be reserved before the ffmpeg child exists.
    pub fn encoder_name(&self) -> &'static str {
        self.software_encoder.unwrap_or("libx264")
    }
}

pub trait Encoder: Send {
    fn submit_video(&mut self, frame: &Frame) -> anyhow::Result<()>;
    fn submit_audio(&mut self, audio: &AudioBuffer) -> anyhow::Result<()>;
    fn finish(&mut self) -> anyhow::Result<()>;
    /// Codec actually in use, for ffprobe assertions and the UI.
    fn active_encoder(&self) -> &'static str;
    /// The frame size this encoder's raw video input was declared with —
    /// [`EncodeConfig::source_size`].
    ///
    /// It is not a hint. The input is a flat byte stream that the encoder's ffmpeg child
    /// slices into frames of exactly this size, so a frame of any other geometry is not
    /// rejected by anything: it is *mis-read*, and the picture comes apart in bands while
    /// the segment files still look healthy. A caller that pumps capture into this
    /// encoder compares each frame against this value and refuses a mismatch out loud
    /// (see `pump_once_counted` in the CLI), because the failure is otherwise silent.
    fn source_size(&self) -> (u32, u32);

    /// Video frames discarded because the encoder could not keep up.
    ///
    /// A live capture has no way to slow the world down: when the encoder's queue is
    /// full the correct behaviour is to drop the frame and carry on (see the queue note
    /// in [`crate::ffmpeg`]), not to block the capture loop and not to fail. Dropping is
    /// only defensible if it is *counted*, so a soak can tell a clean run from one that
    /// silently lost a third of its frames — hence this accessor, which the CLI logs.
    ///
    /// Defaults to 0 for encoders that queue nothing. It counts video frames only;
    /// audio drops are reported separately by [`Encoder::dropped_audio_blocks`].
    fn dropped_frames(&self) -> u64 {
        0
    }

    /// Audio blocks discarded because the encoder could not keep up — see
    /// [`Encoder::dropped_frames`], which this mirrors for the other stream.
    fn dropped_audio_blocks(&self) -> u64 {
        0
    }
}
