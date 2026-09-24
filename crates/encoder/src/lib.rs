//! Hardware video encoding through one long-lived ffmpeg child process.

use localplay_capture::{AudioBuffer, Frame};
use std::path::PathBuf;

pub mod ffmpeg;
pub mod probe;
pub mod throughput;

pub use ffmpeg::{video_input_args, video_output_args, FfmpegEncoder};
pub use probe::select_vendor;
pub use throughput::{frames_per_second, measure_sustainable_fps, ThroughputMeasurement};

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

/// The PCM sample encoding an audio input is declared with (`-f <name>`).
///
/// One variant today, and deliberately a type rather than a bare string: the audio
/// timeline is derived from the byte count, so a format the pipeline does not actually
/// produce is not "unsupported", it is a silently wrong timeline. Every capture backend in
/// this workspace publishes signed 16-bit little-endian PCM
/// (`localplay_capture::AudioFormat::default`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// `s16le` — signed 16-bit little-endian, what `localplay-capture` delivers.
    S16Le,
}

impl SampleFormat {
    /// The ffmpeg demuxer name for this format, as it goes into `-f`.
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            SampleFormat::S16Le => "s16le",
        }
    }
}

/// The format of the **second, optional** audio input: the microphone.
///
/// Declared on [`EncodeConfig::mic_audio`], which is what turns the second input on at all.
/// The three values are not decoration — each one is a number ffmpeg is *told*, and the
/// microphone track is muxed onto the same timeline as the game audio and the picture:
///
/// * `sample_rate` — `-ar`. PCM submitted at 48kHz but declared as 44.1kHz plays back 8.8%
///   fast and slides against the other track for the whole clip;
/// * `channels` — `-ac`. A mono block declared as stereo is read as half its duration;
/// * `sample_format` — `-f`. See [`SampleFormat`].
///
/// [`EncodeConfig::mic_audio`] being `None` is the default and the shipping configuration:
/// the microphone is opted into explicitly, by a caller that sets it on the configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicAudioSpec {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
}

impl Default for MicAudioSpec {
    /// What every capture backend in this workspace publishes: 48kHz stereo s16le
    /// (`localplay_capture::AudioFormat::default`), i.e. the values the game-audio input
    /// has always been declared with.
    fn default() -> Self {
        Self { sample_rate: 48_000, channels: 2, sample_format: SampleFormat::S16Le }
    }
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
    /// When `Some`, a second audio input is declared and mapped as the container's
    /// second audio track (`Microphone`); the first stays `Game Audio`.
    /// When `None`, the argument list is BYTE-IDENTICAL to the pre-change list.
    ///
    /// The microphone is opt-in (`None` is the default in both constructors) and it is a
    /// *second loopback listener* in the encoder child's argument list, not a second pass
    /// over the existing one: the encoder binds the port, owns the pump thread and feeds
    /// ffmpeg's input 2, exactly as it already does for the game audio on input 1. The
    /// caller never dials anything — it hands PCM to
    /// [`Encoder::submit_mic_audio`] and reads the port back with [`Encoder::mic_port`].
    ///
    /// A configured microphone that cannot be brought up (no port to bind, a child that
    /// never dials it) **fails the start**; it never degrades to a recording that claims
    /// two tracks and carries one.
    pub mic_audio: Option<MicAudioSpec>,
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
            // Off unless a caller sets it: the microphone is explicit opt-in, and a
            // config file that says nothing about it must produce the one-track
            // argument list this encoder has always built.
            mic_audio: None,
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
            mic_audio: None,
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
    /// Hand a captured frame to the encoder for encoding.
    ///
    /// Takes the `Frame` by value so the implementation can *move* the pixel buffer
    /// into its writer queue. A `&Frame` forces a clone of `frame.data`, and at
    /// 3840x2160 BGRA that is a 33.2MB memcpy per frame — ~1GB/s of pure copying at
    /// 30fps, on the very path that has to keep up with a live capture (see
    /// `localplay_encoder::ffmpeg`).
    fn submit_video(&mut self, frame: Frame) -> anyhow::Result<()>;
    /// Hand captured PCM to the encoder. Owned for the same reason as
    /// [`Encoder::submit_video`]: the audio block is moved into the writer queue
    /// instead of being cloned into it.
    fn submit_audio(&mut self, audio: AudioBuffer) -> anyhow::Result<()>;

    /// Hand captured **microphone** PCM to the encoder — the second audio track.
    ///
    /// Only an encoder spawned with a microphone ([`EncodeConfig::mic_audio`]) accepts these
    /// blocks, and the two audio inputs are **never mixed**: the block goes to the pump that
    /// feeds ffmpeg's microphone input, exactly as [`Encoder::submit_audio`] feeds the game
    /// audio one, so the two tracks keep their own tones and their own sample counts. The
    /// encoder owns the listener and the pump, so a caller hands PCM over and never dials a
    /// port itself (see [`Encoder::mic_port`]).
    ///
    /// The default implementation is for encoders with no second audio input, and it
    /// **refuses** the block rather than discarding it: quietly dropping a track's worth of
    /// audio would leave a recording that claims two tracks and carries one, which is worse
    /// than a loud failure (same reasoning as the bind failure in `localplay_encoder::ffmpeg`).
    fn submit_mic_audio(&mut self, _audio: AudioBuffer) -> anyhow::Result<()> {
        anyhow::bail!(
            "this encoder has no microphone input (EncodeConfig::mic_audio was None); \
             the block was refused rather than silently dropped"
        )
    }

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

    /// The frame rate this encoder's raw video input was declared with —
    /// [`EncodeConfig::fps`], which is the `-framerate` the child was spawned with
    /// (`localplay_encoder::video_input_args`).
    ///
    /// Read back for the same reason as [`Encoder::source_size`]: it is what the child was
    /// *actually* told, as opposed to what a caller believes it asked for. The engine
    /// builds its [`crate::pump::FramePacer`] from this value (see
    /// `localplay_recorder::Recorder::start_with_measure`), so the rate the capture loop
    /// admits is, by construction, the rate the encoder is encoding at — the two agreeing
    /// is the fix for the measured 4K defect where the pipeline declared a rate it could
    /// not deliver (issues #1 and #2).
    fn input_fps(&self) -> u32;

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

    /// **Microphone** audio blocks discarded because the encoder could not keep up — the
    /// microphone's counterpart of [`Encoder::dropped_audio_blocks`]. Zero for every encoder
    /// that has no second audio input.
    fn dropped_mic_audio_blocks(&self) -> u64 {
        0
    }

    /// The loopback port the microphone input is listening on, or `None` when this encoder
    /// has no microphone input.
    ///
    /// The encoder owns that listener and its pump — a caller hands PCM to
    /// [`Encoder::submit_mic_audio`] and never dials a port itself — so this accessor exists
    /// for the two things a caller can honestly do with the port: log which one the second
    /// input was bound to, and tell "microphone enabled" apart from "microphone requested but
    /// not wired up".
    fn mic_port(&self) -> Option<u16> {
        None
    }
}
