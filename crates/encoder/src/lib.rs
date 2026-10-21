//! Hardware video encoding through one long-lived ffmpeg child process.

// Test bodies may unwrap and may chunk by a constant; production code may do neither.
// Scoped to the test build of this crate, so it cannot hide either in shipped code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks))]

use replayed_capture::{AudioBuffer, Frame};
use std::path::PathBuf;

pub mod ffmpeg;
/// Media Foundation hardware encoders. Windows only, inert elsewhere.
#[cfg(windows)]
pub mod mft;
pub mod probe;
pub mod throughput;

pub use ffmpeg::{video_input_args, video_output_args, FfmpegEncoder};
pub use probe::select_vendor;
pub use throughput::{
    frames_per_second, measure_sustainable_fps, measured_path, MeasuredPath, ThroughputMeasurement,
};

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

    /// The codec an ffmpeg encoder name belongs to.
    ///
    /// The name is the only place the codec is visible: the recorder publishes the encoder's
    /// name (`h264_nvenc`), and a panel that has to say "H.264" cannot read it out of that.
    pub fn of_encoder(encoder: &str) -> Option<Self> {
        if encoder.starts_with("h264") {
            Some(VideoCodec::H264)
        } else if encoder.starts_with("hevc") {
            Some(VideoCodec::Hevc)
        } else {
            None
        }
    }

    /// The codec's own name, as the config file spells it.
    pub fn as_str(self) -> &'static str {
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

impl Vendor {
    /// The name `encode.vendor` uses for this vendor.
    pub fn as_str(self) -> &'static str {
        match self {
            Vendor::Nvenc => "nvenc",
            Vendor::Qsv => "qsv",
            Vendor::Amf => "amf",
        }
    }
}

/// The family an encoder name belongs to, and what it can be asked for.
///
/// The families spell the same settings differently: NVENC, QSV and AMF each have their own
/// rate-control switch and their own preset names, so a table keyed by the family is the only
/// way two places (the argument list and the settings panel) cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderFamily {
    Nvenc,
    Qsv,
    Amf,
    /// A software encoder (libx264/libx265). Only the test builds reach it; there is no shipping
    /// CPU fallback.
    Software,
}

impl EncoderFamily {
    /// The family an ffmpeg encoder name belongs to.
    ///
    /// By suffix, which is how ffmpeg names them: `h264_nvenc`, `hevc_qsv`, `h264_amf`. Anything
    /// else is software.
    pub fn of(encoder: &str) -> Self {
        if encoder.ends_with("_nvenc") {
            EncoderFamily::Nvenc
        } else if encoder.ends_with("_qsv") {
            EncoderFamily::Qsv
        } else if encoder.ends_with("_amf") {
            EncoderFamily::Amf
        } else {
            EncoderFamily::Software
        }
    }

    /// The family a vendor name belongs to, as `encode.vendor` spells it.
    ///
    /// The other direction of [`EncoderFamily::of`]: the config names a vendor, the encoder
    /// names a family, and the settings panel has to know which presets the vendor takes.
    pub fn of_vendor(vendor: &str) -> Option<Self> {
        match vendor {
            "nvenc" => Some(EncoderFamily::Nvenc),
            "qsv" => Some(EncoderFamily::Qsv),
            "amf" => Some(EncoderFamily::Amf),
            _ => None,
        }
    }

    /// The vendor this family is, or `None` for the software one.
    pub fn vendor(self) -> Option<Vendor> {
        match self {
            EncoderFamily::Nvenc => Some(Vendor::Nvenc),
            EncoderFamily::Qsv => Some(Vendor::Qsv),
            EncoderFamily::Amf => Some(Vendor::Amf),
            EncoderFamily::Software => None,
        }
    }

    /// The presets this family accepts (ffmpeg's `-preset` values).
    ///
    /// The names are ffmpeg's, not ours: a preset that is not one of these is not passed on at
    /// all (ffmpeg refuses an unknown value and the recording would not start), so this list is
    /// also what the settings panel may offer.
    pub fn presets(self) -> &'static [&'static str] {
        match self {
            // NVENC: p1 is the slowest and the best, p7 the fastest.
            EncoderFamily::Nvenc => &["p1", "p2", "p3", "p4", "p5", "p6", "p7"],
            EncoderFamily::Qsv => &[
                "veryfast", "faster", "fast", "medium", "slow", "slower", "veryslow",
            ],
            EncoderFamily::Amf => &["speed", "balanced", "quality"],
            EncoderFamily::Software => &[
                "ultrafast",
                "superfast",
                "veryfast",
                "faster",
                "fast",
                "medium",
                "slow",
                "slower",
                "veryslow",
            ],
        }
    }
}

/// The rate-control mode an encoder is asked for.
///
/// Three modes, because three is what the encoders this app drives really offer:
///
/// * [`RateControl::Cbr`]: a rate to hold. What every build before this existed did.
/// * [`RateControl::Vbr`]: a rate to aim near, with the peak bounded.
/// * [`RateControl::Quality`]: a constant quality (a quantiser value). **At the encoder's own
///   value**: this build carries no QP setting, so the number is whatever the encoder defaults
///   to, and the file's size follows the picture rather than the configured bitrate.
///
/// **Not every path can do every mode.** The ffmpeg-child path can do all three (the vendor's own
/// constant-quality mode); the Media Foundation path reaches CBR and VBR through
/// `CODECAPI_AVEncCommonRateControlMode`, and its constant-quality mode needs a QP value this
/// app does not carry — see [`MFT_RATE_CONTROLS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RateControl {
    #[default]
    Cbr,
    Vbr,
    Quality,
}

impl RateControl {
    /// Every mode the ffmpeg-child path can be told to use, in the order a panel lists them.
    pub const ALL: [RateControl; 3] = [RateControl::Cbr, RateControl::Vbr, RateControl::Quality];

    /// The name in `config.toml` and in the settings panel.
    pub fn as_str(self) -> &'static str {
        match self {
            RateControl::Cbr => "cbr",
            RateControl::Vbr => "vbr",
            RateControl::Quality => "quality",
        }
    }

    /// The mode a configuration asks for.
    ///
    /// The error names what is accepted, because a hand-edited file with `rate_control = "crf"`
    /// must say what to write instead — and `crf` is a software-encoder mode this app has no
    /// encoder for.
    pub fn parse(spec: &str) -> anyhow::Result<Self> {
        match spec.trim().to_ascii_lowercase().as_str() {
            "cbr" => Ok(RateControl::Cbr),
            "vbr" => Ok(RateControl::Vbr),
            "quality" => Ok(RateControl::Quality),
            other => anyhow::bail!(
                "unknown encode.rate_control {other:?}: expected \"cbr\" (a rate to hold), \
                 \"vbr\" (a rate to aim near) or \"quality\" (a constant quality)"
            ),
        }
    }
}

/// The rate-control modes the Media Foundation (zero-copy) path can be told to use.
///
/// The MFT's mode comes from `CODECAPI_AVEncCommonRateControlMode`, which has constant bitrate
/// and the variable-bitrate family. Its constant-quality mode needs a QP value this app does not
/// carry, so it is **not** in this list: the list is what the settings panel may offer, and it
/// must not offer a mode the encoder cannot be told to use.
pub const MFT_RATE_CONTROLS: [RateControl; 2] = [RateControl::Cbr, RateControl::Vbr];

/// The Media Foundation code for "a stream change has occurred. Output cannot be produced until
/// the streams have been renegotiated" (`MF_E_TRANSFORM_STREAM_CHANGE`).
///
/// The SDK's name for it is `MF_E_TRANSFORM_STREAM_CHANGE`; there is no `MF_E_TRANSFORM_TYPE_CHANGE`
/// in the SDK, and 0xC00D6D61 is this code. It is a number here because the `windows` crate's
/// `HRESULT` is not available off Windows, and the decision it drives has to be testable there.
pub(crate) const MF_STREAM_CHANGE: u32 = 0xC00D6D61;

/// The Media Foundation code for "the transform needs more input" (`MF_E_TRANSFORM_NEED_MORE_INPUT`).
pub(crate) const MF_NEED_MORE_INPUT: u32 = 0xC00D6D72;

/// What an error from an encoder's output call means, and what to do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFailure {
    /// It has no output yet and wants more input. Not a failure at all: asking before the
    /// encoder has anything is the documented answer.
    NeedsInput,
    /// It changed its output type. The streams have to be renegotiated before anything can be
    /// produced, and asking again afterwards is what produces the frame.
    TypeChanged,
    /// Something else went wrong. The driver's own words are the report.
    Failed,
}

/// What an error code from an encoder's output call means.
///
/// The classification is here, and not in the Windows-only module, because it is the decision the
/// bug was in: a type change is not a failure to report but a renegotiation to perform, and a
/// caller that cannot tell the two apart can do neither.
pub fn classify_output_failure(code: u32) -> OutputFailure {
    match code {
        MF_NEED_MORE_INPUT => OutputFailure::NeedsInput,
        MF_STREAM_CHANGE => OutputFailure::TypeChanged,
        _ => OutputFailure::Failed,
    }
}

/// An output type an encoder offers, in the terms the choice between types is made in.
///
/// Plain data on purpose. The Media Foundation calls that produce these are Windows-only, but the
/// choice between them is not, and a choice that can only be made on one platform is a choice
/// that is only ever tested on one platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputTypeSpec {
    /// The subtype's name, as the codec spells it (`"h264"`) or as the encoder's registry spells
    /// it (`"H264"`). Compared without regard to case, because both spellings are the SDK's and
    /// the codec's, not this app's.
    pub subtype: String,
    /// The frame size the type encodes.
    pub size: (u32, u32),
    /// The frame rate the type declares.
    pub fps: u32,
}

/// Which of the offered output types to set, when an encoder says it changed its type.
///
/// The rules, and why each is what it is:
///
/// * the **codec and the size are requirements**: a type for another codec is not this
///   recording's stream, and a type at another size does not describe the texture that goes in;
/// * the **frame rate is a preference**: an encoder that changed its type may have settled on a
///   rate of its own, and the recording follows the encoder rather than arguing with it.
///
/// `None` when no offer is usable. The caller must then refuse out loud rather than set a type
/// it cannot encode with: a stream in the wrong codec or at the wrong size is not a recording.
pub fn choose_output_type(offers: &[OutputTypeSpec], wanted: &OutputTypeSpec) -> Option<usize> {
    let usable = |offer: &OutputTypeSpec| {
        offer.subtype.eq_ignore_ascii_case(&wanted.subtype) && offer.size == wanted.size
    };
    let first = offers.iter().position(usable)?;
    // Two types can share a codec and a size and differ only in the rate they declare, so the
    // rate the recording asked for wins whenever the encoder offers it.
    offers
        .iter()
        .position(|offer| usable(offer) && offer.fps == wanted.fps)
        .or(Some(first))
}

/// What to say when an encoder changed its output type and none of the types it offers is usable.
///
/// A message rather than an error: the caller holds the driver's own words for why the set
/// failed, and this says what was needed and what was on offer, which is what makes the refusal
/// readable rather than merely a refusal.
pub fn type_change_refusal(wanted: &OutputTypeSpec, offers: &[OutputTypeSpec]) -> String {
    let offered = if offers.is_empty() {
        "no output type at all".to_string()
    } else {
        offers
            .iter()
            .map(|offer| {
                format!(
                    "{} {}x{} at {}fps",
                    offer.subtype, offer.size.0, offer.size.1, offer.fps
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "the encoder changed its output type and the stream cannot be renegotiated: it needs {} \
         at {}x{}, and it offers {offered}",
        wanted.subtype, wanted.size.0, wanted.size.1
    )
}

/// The PCM sample encoding an audio input is declared with (`-f <name>`).
///
/// A type rather than a bare string: the audio timeline is derived from the byte count, so a
/// format the pipeline does not produce yields a silently wrong timeline. Every capture backend
/// publishes signed 16-bit little-endian PCM (`replayed_capture::AudioFormat::default`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// Signed 16-bit little-endian, what `replayed-capture` delivers.
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
    /// What every capture backend publishes: 48kHz stereo s16le (`replayed_capture::AudioFormat::default`).
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
    /// It cannot stand alone: `replayed_media::edit::audio_titles` names a one-track container
    /// `Game Audio`, so `game_audio = false` with a microphone is refused at spawn.
    pub mic_audio: Option<MicAudioSpec>,
    /// `None` means the real hardware encoder, the only shipping configuration.
    pub(crate) software_encoder: Option<&'static str>,
    /// Where the encoded stream goes: files on disk, or a fragmented-MP4 pipe. Defaults to [`EncodeOutput::Segmented`].
    pub output: EncodeOutput,
    /// Where the video comes from: this process's own pixels, or its own encoder. Defaults to [`VideoInput::RawPixels`].
    pub video: VideoInput,
    /// The rate-control mode the encoder is asked for. Defaults to [`RateControl::Cbr`], which is
    /// what every build before this setting existed did.
    pub rate_control: RateControl,
    /// The encoder's own preset (ffmpeg's `-preset`); empty means the encoder's own default.
    ///
    /// Only the ffmpeg-child path can use one: the Media Foundation path has no generic preset
    /// (`ICodecAPI` has no such key), so a preset set there is dropped with a warning.
    pub preset: String,
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

    /// Ask for a rate-control mode and an encoder preset.
    ///
    /// A builder rather than two more constructor arguments: the constructor already carries
    /// eight, and these two are optional refinements of an otherwise complete configuration.
    pub fn with_rate_control(mut self, mode: RateControl, preset: impl Into<String>) -> Self {
        self.rate_control = mode;
        self.preset = preset.into();
        self
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
    /// The caller keeps the stream in memory (`replayed_replay`'s `MemoryRingBuffer`), so an idle
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
            // Constant bitrate, as every build before these two settings existed.
            rate_control: RateControl::default(),
            // The encoder's own default preset.
            preset: String::new(),
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
            // Constant bitrate, as every build before these two settings existed.
            rate_control: RateControl::default(),
            // The encoder's own default preset.
            preset: String::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The mode a configuration asks for, and what an unknown one says.
    ///
    /// The error has to name the accepted values: a hand-edited `rate_control = "crf"` must say
    /// what to write instead, and "crf" is a software-encoder mode this app has no encoder for.
    #[test]
    fn the_rate_control_mode_parses_and_an_unknown_one_says_what_is_accepted() {
        assert_eq!(RateControl::parse("cbr").unwrap(), RateControl::Cbr);
        assert_eq!(RateControl::parse("vbr").unwrap(), RateControl::Vbr);
        assert_eq!(RateControl::parse("quality").unwrap(), RateControl::Quality);
        // The spelling is forgiving about case and space, like the rest of the config.
        assert_eq!(RateControl::parse("  CBR ").unwrap(), RateControl::Cbr);

        let err = RateControl::parse("crf").expect_err("crf is not a mode this build has");
        let message = err.to_string();
        assert!(
            message.contains("crf"),
            "the message names the value: {message}"
        );
        assert!(
            message.contains("cbr") && message.contains("vbr") && message.contains("quality"),
            "and the modes that are accepted: {message}"
        );
    }

    /// The mode's name is what the config file and the panel use.
    #[test]
    fn the_modes_keep_their_names() {
        assert_eq!(RateControl::Cbr.as_str(), "cbr");
        assert_eq!(RateControl::Vbr.as_str(), "vbr");
        assert_eq!(RateControl::Quality.as_str(), "quality");
        // And the default is the mode every build before this existed used.
        assert_eq!(RateControl::default(), RateControl::Cbr);
    }

    /// The family an encoder name belongs to, and the vendor behind it.
    ///
    /// The three hardware families spell the same settings differently, so the tables that list
    /// them have to be keyed by the family and not by the vendor string in the config.
    #[test]
    fn the_encoder_family_follows_the_name() {
        assert_eq!(EncoderFamily::of("h264_nvenc"), EncoderFamily::Nvenc);
        assert_eq!(EncoderFamily::of("hevc_nvenc"), EncoderFamily::Nvenc);
        assert_eq!(EncoderFamily::of("h264_qsv"), EncoderFamily::Qsv);
        assert_eq!(EncoderFamily::of("hevc_amf"), EncoderFamily::Amf);
        assert_eq!(EncoderFamily::of("libx264"), EncoderFamily::Software);
        assert_eq!(EncoderFamily::Nvenc.vendor(), Some(Vendor::Nvenc));
        assert_eq!(EncoderFamily::Amf.vendor().map(Vendor::as_str), Some("amf"));
        assert_eq!(EncoderFamily::Software.vendor(), None);
    }

    /// Every family has presets, and no two families share a name.
    ///
    /// A preset that belongs to another family is not passed on at all (ffmpeg refuses an
    /// unknown `-preset` and the recording would not start), so the lists must not overlap.
    #[test]
    fn the_preset_lists_are_per_family_and_do_not_overlap() {
        let nvenc = EncoderFamily::Nvenc.presets();
        let qsv = EncoderFamily::Qsv.presets();
        let amf = EncoderFamily::Amf.presets();
        assert!(nvenc.contains(&"p4"));
        assert!(qsv.contains(&"medium"));
        assert!(amf.contains(&"balanced"));
        for name in nvenc {
            assert!(!qsv.contains(name), "{name} is in two families' lists");
            assert!(!amf.contains(name), "{name} is in two families' lists");
        }
    }

    // ---- the output type to set when an encoder says it changed its type ---------------

    /// A type change is a renegotiation, not a failure.
    ///
    /// This is the decision the bug was in: an output call answering "a stream change has
    /// occurred" (0xC00D6D61) used to travel the same road as every other error, so the probe
    /// could not measure on a machine whose encoder renegotiates, and the recording stopped
    /// encoding on one.
    #[test]
    fn a_type_change_is_a_renegotiation_and_not_a_failure() {
        assert_eq!(
            classify_output_failure(MF_STREAM_CHANGE),
            OutputFailure::TypeChanged,
            "the type change has to be told apart from the failures, or nothing renegotiates it"
        );
        assert_eq!(
            classify_output_failure(MF_NEED_MORE_INPUT),
            OutputFailure::NeedsInput,
            "asking before the encoder has output is not a failure"
        );
        assert_eq!(
            classify_output_failure(0xC00D6D60),
            OutputFailure::Failed,
            "anything else is the driver's own words to report"
        );
    }

    /// The type the recording needs, as the codec spells it.
    fn wanted_h264() -> OutputTypeSpec {
        OutputTypeSpec {
            subtype: "h264".to_string(),
            size: (2560, 1080),
            fps: 60,
        }
    }

    /// An offered type, in the encoder's own spelling.
    fn offer(subtype: &str, size: (u32, u32), fps: u32) -> OutputTypeSpec {
        OutputTypeSpec {
            subtype: subtype.to_string(),
            size,
            fps,
        }
    }

    /// The type that matches the codec and the size is the one to set, and the rate the
    /// recording asked for wins over another rate at the same codec and size.
    ///
    /// The codec and the size are requirements: a type at another size does not describe the
    /// texture that goes in, and a type for another codec is not this recording's stream. The
    /// rate is a preference, because an encoder that changed its type may have settled on a rate
    /// of its own.
    #[test]
    fn the_output_type_to_set_is_the_one_that_matches_the_codec_and_the_size() {
        let wanted = wanted_h264();
        let offers = vec![
            offer("H264", (1920, 1080), 60),
            offer("H264", (2560, 1080), 30),
            offer("H264", (2560, 1080), 60),
        ];
        assert_eq!(
            choose_output_type(&offers, &wanted),
            Some(2),
            "the type at the wanted size, declaring the wanted rate, is the one to set"
        );
    }

    /// Nothing usable is nothing chosen: the caller has to refuse rather than set a type the
    /// encoder will not encode with.
    #[test]
    fn no_usable_offer_means_no_type_is_chosen() {
        let wanted = wanted_h264();
        // Another size: the texture does not fit it.
        assert_eq!(
            choose_output_type(&[offer("H264", (1920, 1080), 60)], &wanted),
            None,
            "a type at another size does not describe the texture that goes in"
        );
        // Another codec: not this recording's stream.
        assert_eq!(
            choose_output_type(&[offer("HEVC", (2560, 1080), 60)], &wanted),
            None,
            "a type for another codec is not this recording's stream"
        );
        // Nothing offered at all.
        assert_eq!(
            choose_output_type(&[], &wanted),
            None,
            "an encoder that offers no type cannot be renegotiated"
        );
    }

    /// The subtype's spelling is the SDK's and the codec's, not this app's: the registry says
    /// "H264" and the codec says "h264", and they are the same type.
    #[test]
    fn the_subtypes_spelling_does_not_have_to_match_exactly() {
        let wanted = wanted_h264();
        assert_eq!(
            choose_output_type(&[offer("H264", (2560, 1080), 60)], &wanted),
            Some(0),
            "the registry's spelling of the codec's subtype is the same type"
        );
    }

    /// A refusal has to say what was needed and what was on offer, or it is only a refusal.
    #[test]
    fn the_refusal_names_what_was_needed_and_what_was_offered() {
        let wanted = wanted_h264();
        let offers = vec![
            offer("H264", (1920, 1080), 60),
            offer("HEVC", (2560, 1080), 60),
        ];
        let message = type_change_refusal(&wanted, &offers);
        assert!(
            message.contains("h264") && message.contains("2560x1080"),
            "the message names what the recording needs: {message}"
        );
        assert!(
            message.contains("1920x1080") && message.contains("HEVC"),
            "and what was on offer instead: {message}"
        );

        let nothing = type_change_refusal(&wanted, &[]);
        assert!(
            nothing.contains("no output type at all"),
            "an encoder that offers nothing says so rather than listing nothing: {nothing}"
        );
    }
}
