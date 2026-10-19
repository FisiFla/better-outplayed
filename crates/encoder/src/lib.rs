//! Hardware video encoding through one long-lived ffmpeg child process.

// Test bodies may unwrap and may chunk by a constant; production code may do neither.
// Scoped to the test build of this crate, so it cannot hide either in shipped code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks))]

use localplay_capture::{AudioBuffer, Frame};
use std::path::PathBuf;

pub mod ffmpeg;
/// Media Foundation hardware encoders. Windows only, inert elsewhere.
#[cfg(windows)]
pub mod mft;
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

    /// What ffmpeg calls this codec's elementary stream, the name for `-f`.
    ///
    /// Kept beside [`VideoCodec::hw_encoder_name`]: a mismatch is not an error ffmpeg reports, it
    /// misreads the framing and produces a broken recording.
    pub fn bitstream_format(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::Hevc => "hevc",
        }
    }
}

/// GPU encoder vendors. There is deliberately no software variant: a silent CPU fallback is
/// not allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Nvenc,
    Qsv,
    Amf,
}

/// The PCM sample encoding an audio input is declared with (`-f <name>`).
///
/// A type rather than a bare string: the audio timeline is derived from the byte count, so a
/// format the pipeline does not produce yields a silently wrong timeline. Every capture backend
/// publishes signed 16-bit little-endian PCM (`localplay_capture::AudioFormat::default`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// Signed 16-bit little-endian, what `localplay-capture` delivers.
    S16Le,
}

impl SampleFormat {
    /// The ffmpeg demuxer name, as it goes into `-f`.
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            SampleFormat::S16Le => "s16le",
        }
    }
}

/// The format of the optional second audio input: the microphone.
///
/// Declared on [`EncodeConfig::mic_audio`]. Each value is a number ffmpeg is told, and a wrong
/// one desyncs the track from the others:
///
/// * `sample_rate` is `-ar`: 48kHz PCM declared as 44.1kHz plays back 8.8% fast;
/// * `channels` is `-ac`: a mono block declared as stereo is read as half its duration;
/// * `sample_format` is `-f`, see [`SampleFormat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicAudioSpec {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
}

impl Default for MicAudioSpec {
    /// What every capture backend publishes: 48kHz stereo s16le (`localplay_capture::AudioFormat::default`).
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            channels: 2,
            sample_format: SampleFormat::S16Le,
        }
    }
}

/// The game track's bitrate, in kbps, when nothing carries one.
///
/// `[audio] bitrate_kbps` is the setting; the recorder falls back to this when a front-end
/// carries no value (the desktop shell does not parse `[audio]`). One constant so two crates
/// cannot drift.
pub const DEFAULT_AUDIO_BITRATE_KBPS: u32 = 192;

#[derive(Debug, Clone)]
pub struct EncodeConfig {
    pub codec: VideoCodec,
    /// The frame size on the rawvideo pipe, the capture backend's native size. ffmpeg's `-s` is set
    /// from this; if it is wrong the pipe desyncs.
    pub source_size: (u32, u32),
    /// The encoded output size. When it differs from `source_size` the encoder inserts a `scale` filter.
    pub output_size: (u32, u32),
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub segment_ms: u64,
    pub scratch_dir: PathBuf,
    pub audio_bitrate_kbps: u32,
    /// Sequence number of the first segment this encoder writes (`seg-%06d.mp4`).
    ///
    /// ffmpeg's segment muxer numbers files from 0 every time it is spawned, so a second run would
    /// overwrite the file the adopted ledger still points at. The caller passes the first free
    /// number (the ledger's reserved segment number). Default `0`: an empty scratch directory.
    pub start_number: u64,
    /// Whether the game-audio track is recorded at all.
    ///
    /// `false` declares no audio input at all: no listener is bound, no `-c:a` is passed and the
    /// container carries no audio stream (`[audio] enabled = false`).
    pub game_audio: bool,
    /// When `Some`, a second audio input is declared and mapped as the container's second audio
    /// track (`Microphone`); the first stays `Game Audio`. When `None` the argument list is the
    /// one-track list, byte for byte.
    ///
    /// The encoder binds the port, owns the pump thread and feeds ffmpeg's input 2, as it does for
    /// the game audio on input 1. The caller hands PCM to [`Encoder::submit_mic_audio`] and reads
    /// the port back with [`Encoder::mic_port`].
    ///
    /// A configured microphone that cannot be brought up fails the start; it never degrades to a
    /// recording that claims two tracks and carries one.
    ///
    /// It cannot stand alone: `localplay_media::edit::audio_titles` names a one-track container
    /// `Game Audio`, so `game_audio = false` with a microphone is refused at spawn.
    pub mic_audio: Option<MicAudioSpec>,
    /// `None` means the real hardware encoder, the only shipping configuration.
    pub(crate) software_encoder: Option<&'static str>,
    /// Where the encoded stream goes: files on disk, or a fragmented-MP4 pipe. Defaults to [`EncodeOutput::Segmented`].
    pub output: EncodeOutput,
    /// Where the video comes from: this process's own pixels, or its own encoder. Defaults to [`VideoInput::RawPixels`].
    pub video: VideoInput,
}

/// Where an encoder's output goes.
///
/// A storage choice, not a codec one: both modes run the same ffmpeg, encoder and forced-keyframe
/// interval, and both produce footage a clip can be cut from with `-c copy`. They differ in
/// whether buffered footage is on disk.
impl EncodeConfig {
    /// Whether this configuration's encoder takes GPU textures rather than pixels.
    ///
    /// One function so a caller deciding whether to ask capture for textures
    /// (`CaptureBackend::deliver_textures`) and an encoder deciding whether it can consume one
    /// cannot disagree.
    pub fn accepts_textures(&self) -> bool {
        self.video == VideoInput::EncodedBitstream
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoInput {
    /// Raw BGRA frames on the child's stdin, at the declared rate. What every test and non-Windows
    /// build uses.
    ///
    /// Costly at 4K: the capture backend copies 33.2 MB out of VRAM per frame, ~97% of the pipeline's work.
    RawPixels,
    /// An H.264 elementary stream this process encoded itself, on the child's stdin.
    ///
    /// The hybrid path: a hardware encoder MFT takes the captured texture straight from VRAM
    /// (`crate::mft`), so ffmpeg receives a few hundred kilobytes a second instead of gigabytes of
    /// raw pixels, and is told to copy the video rather than encode it. Consequences the argument
    /// list enforces:
    ///
    /// * The GOP is the MFT's, not ffmpeg's: `-force_key_frames` cannot apply to a copied stream,
    ///   so segment boundaries come from the MFT's own keyframe interval.
    /// * The output cannot be scaled: that needs decoding, which is the cost this variant removes.
    ///   A scaled `output_size` is refused at spawn.
    EncodedBitstream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeOutput {
    /// `-f segment` into [`EncodeConfig::scratch_dir`]: one self-contained file per `segment_ms`,
    /// each starting at zero.
    ///
    /// Survives a crash and is what session mode needs, but costs SSD writes while the user is only
    /// waiting for something to clip (see [`EncodeOutput::FragmentedStream`]).
    Segmented,
    /// Fragmented MP4 on the child's stdout (`pipe:1`): one `moof`+`mdat` fragment per forced
    /// keyframe, and no file written at all.
    ///
    /// The caller keeps the stream in memory (`localplay_replay`'s `MemoryRingBuffer`), so an idle
    /// replay buffer touches the disk only when a clip is saved. How it works:
    ///
    /// * `empty_moov+frag_keyframe+default_base_moof` gives a small `ftyp`+`moov` header (1249 bytes
    ///   for a 6s 320x180 capture) then one `moof`+`mdat` per fragment, so any contiguous range of
    ///   fragments appended to the kept header is a valid fragmented MP4;
    /// * `frag_keyframe` puts a boundary on every forced keyframe, so `segment_ms` stays the cut
    ///   granularity;
    /// * each fragment's `tfdt` (in the track timescale) is monotonic and gives its
    ///   `[start_ms, end_ms)` without decoding.
    FragmentedStream,
}

impl EncodeConfig {
    #[allow(clippy::too_many_arguments)]
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
            audio_bitrate_kbps: DEFAULT_AUDIO_BITRATE_KBPS,
            // A first run (and every test) writes `seg-000000.mp4`; the CLI overrides this from what is on disk.
            start_number: 0,
            // Off unless a caller sets it: the microphone is explicit opt-in.
            mic_audio: None,
            // On unless a caller that has read `[audio] enabled` says otherwise (today the CLI alone).
            game_audio: true,
            software_encoder: Some(codec.hw_encoder_name(vendor)),
            // Files on disk, which survive a crash. A caller wanting no disk churn sets `FragmentedStream`.
            output: EncodeOutput::Segmented,
            video: VideoInput::RawPixels,
        }
    }

    /// Only available to tests and dev builds. The CLI cannot construct this from a user config
    /// file, which keeps "no CPU fallback" honest.
    ///
    /// Source and output are the same size, so no scaling is involved.
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
            // On, like the shipping constructor; a video-only test sets it to `false` itself.
            game_audio: true,
            software_encoder: None,
            output: EncodeOutput::Segmented,
            video: VideoInput::RawPixels,
        }
    }

    /// The ffmpeg encoder this configuration selects, the same string [`Encoder::active_encoder`]
    /// reports once running.
    ///
    /// Public so a caller can name the encoder before spawning it (the CLI records it on every clip).
    pub fn encoder_name(&self) -> &'static str {
        // The fallback is per codec: a software HEVC run must not produce an H.264 file that claims to
        // be HEVC. `software_encoder` is the hardware name and is only set by [`EncodeConfig::hardware`];
        // nothing here names a software encoder on purpose.
        self.software_encoder.unwrap_or(match self.codec {
            VideoCodec::H264 => "libx264",
            VideoCodec::Hevc => "libx265",
        })
    }
}

pub trait Encoder: Send {
    /// Hand a captured frame to the encoder.
    ///
    /// Takes the `Frame` by value so the implementation can move the pixel buffer into its writer
    /// queue; a `&Frame` would force a 33.2MB memcpy per frame at 3840x2160 BGRA.
    fn submit_video(&mut self, frame: Frame) -> anyhow::Result<()>;
    /// Hand captured PCM to the encoder. Owned for the same reason as [`Encoder::submit_video`].
    fn submit_audio(&mut self, audio: AudioBuffer) -> anyhow::Result<()>;

    /// Hand captured microphone PCM to the encoder, the second audio track.
    ///
    /// Only an encoder spawned with [`EncodeConfig::mic_audio`] accepts these blocks. The two audio
    /// inputs are never mixed: each block goes to its own pump, so the tracks keep their own tones
    /// and sample counts. A caller hands PCM over and never dials a port itself (see [`Encoder::mic_port`]).
    ///
    /// The default implementation refuses the block rather than discarding it: quietly dropping a
    /// track would leave a recording that claims two tracks and carries one.
    fn submit_mic_audio(&mut self, _audio: AudioBuffer) -> anyhow::Result<()> {
        anyhow::bail!(
            "this encoder has no microphone input (EncodeConfig::mic_audio was None); \
             the block was refused rather than silently dropped"
        )
    }

    fn finish(&mut self) -> anyhow::Result<()>;
    /// Codec actually in use, for ffprobe assertions and the UI.
    fn active_encoder(&self) -> &'static str;
    /// The frame size this encoder's raw video input was declared with ([`EncodeConfig::source_size`]).
    ///
    /// Not a hint: ffmpeg slices the flat byte stream into frames of exactly this size, so a frame
    /// of any other geometry is mis-read (the picture comes apart in bands while segments look
    /// healthy). Callers compare each frame against it and refuse a mismatch out loud.
    fn source_size(&self) -> (u32, u32);

    /// Whether this encoder can take a frame whose pixels are a GPU texture rather than bytes.
    ///
    /// A property of the configuration, not a running encoder, because capture is told what to
    /// deliver before an encoder exists. Defaults to `false`: a textured frame reaching an encoder
    /// that wants pixels is refused by name (`FfmpegEncoder::submit_video`).
    fn accepts_textures(&self) -> bool {
        false
    }

    /// The frame rate this encoder's raw video input was declared with ([`EncodeConfig::fps`]),
    /// the `-framerate` the child was spawned with.
    ///
    /// Read back because it is what the child was actually told. The engine builds its
    /// [`crate::pump::FramePacer`] from it, so the rate the capture loop admits is by construction
    /// the rate the encoder is encoding at.
    fn input_fps(&self) -> u32;

    /// Video frames discarded because the encoder could not keep up.
    ///
    /// A live capture cannot slow the world down, so when the encoder's queue is full the frame is
    /// dropped (see the queue note in [`crate::ffmpeg`]); dropping is only defensible if counted.
    /// Defaults to 0. Video only; see [`Encoder::dropped_audio_blocks`] for audio.
    fn dropped_frames(&self) -> u64 {
        0
    }

    /// Audio blocks discarded because the encoder could not keep up (mirrors [`Encoder::dropped_frames`]).
    fn dropped_audio_blocks(&self) -> u64 {
        0
    }

    /// Microphone audio blocks discarded because the encoder could not keep up. Zero for encoders
    /// with no second audio input.
    fn dropped_mic_audio_blocks(&self) -> u64 {
        0
    }

    /// The loopback port the microphone input is listening on, or `None` without a microphone input.
    ///
    /// The encoder owns the listener; a caller can log the port and tell "microphone enabled" apart
    /// from "requested but not wired up".
    fn mic_port(&self) -> Option<u16> {
        None
    }

    /// Take the encoder's fragmented-MP4 output stream, when spawned with [`EncodeOutput::FragmentedStream`].
    ///
    /// `None` for a segmented encoder (its output is files), so that cannot be confused with
    /// "somebody already took the stream".
    ///
    /// Taken, not borrowed: a pipe has exactly one reader, the in-memory ring buffer, which
    /// outlives any borrow; a second caller's `None` is honest.
    fn take_output_stream(&mut self) -> Option<std::process::ChildStdout> {
        None
    }
}
