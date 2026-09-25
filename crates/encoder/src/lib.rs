//! Hardware video encoding through one long-lived ffmpeg child process.

use localplay_capture::{AudioBuffer, Frame};
use std::path::PathBuf;

pub mod ffmpeg;
/// Media Foundation hardware encoders: what this machine offers, and whether they take a
/// GPU texture. Windows only, and inert everywhere else.
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
    /// (the ledger's reserved segment number) and passes it here.
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
    /// Where the encoded stream goes: files on disk, or a fragmented-MP4 pipe.
    ///
    /// Defaults to [`EncodeOutput::Segmented`] everywhere, so every existing caller and test
    /// keeps the behaviour it had. See [`EncodeOutput`] for what the other mode is for.
    pub output: EncodeOutput,
    /// Where the video comes from: this process's own pixels, or this process's own encoder.
    ///
    /// Defaults to [`VideoInput::RawPixels`] everywhere, so every existing caller and test keeps the
    /// argument list it had, element for element. See [`VideoInput`] for what the other is for.
    pub video: VideoInput,
}

/// Where an encoder's output goes.
///
/// The choice is a *storage* one, not a codec one: both modes run the same ffmpeg, the same
/// encoder and the same forced-keyframe interval, and both produce footage a clip can be cut
/// from with `-c copy`. What differs is whether the footage is on disk while it is only being
/// *buffered*.
impl EncodeConfig {
    /// Whether this configuration's encoder takes GPU textures rather than pixels.
    ///
    /// One function rather than a `match` at each call site, and the same function the
    /// [`Encoder::accepts_textures`] default delegates to for the configurations this crate builds:
    /// a caller deciding whether to ask capture for textures (`CaptureBackend::deliver_textures`)
    /// and an encoder deciding whether it can consume one must not be able to disagree.
    pub fn accepts_textures(&self) -> bool {
        self.video == VideoInput::EncodedBitstream
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoInput {
    /// Raw BGRA frames on the child's stdin, at the declared rate — every configuration this
    /// project has shipped until now, and what every test and non-Windows build uses.
    ///
    /// The cost of it is the reason the other variant exists: at 3840x2160 the capture backend
    /// copies 33.2 MB out of VRAM per frame to produce these bytes, which is ~97% of what the 4K
    /// pipeline does (§14 of `docs/verification-status.md`).
    RawPixels,
    /// **An H.264 elementary stream this process encoded itself**, on the child's stdin.
    ///
    /// The Tier 2 hybrid: a hardware encoder MFT takes the captured texture straight from VRAM
    /// (`crate::mft`), and what reaches ffmpeg is a few hundred kilobytes a second of H.264 instead
    /// of gigabytes of raw pixels — measured on the box at 4K: 90 frames in, an elementary stream
    /// out, no CPU copy. ffmpeg is then told to **copy** the video rather than encode it.
    ///
    /// Two consequences the argument list enforces rather than assumes, both measured:
    ///
    /// * **The GOP is the encoder's, not ffmpeg's.** `-force_key_frames` cannot apply to a stream
    ///   ffmpeg is only copying, so the segment boundaries the replay path depends on come from the
    ///   MFT's own keyframe interval.
    /// * **The output cannot be scaled here.** Scaling means decoding, and decoding means the
    ///   pixels, which is the cost this variant exists to remove; a scaled `output_size` is refused
    ///   at spawn with that explanation rather than silently producing the capture's size.
    EncodedBitstream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeOutput {
    /// `-f segment` into [`EncodeConfig::scratch_dir`]: one self-contained file per
    /// `segment_ms`, each starting at zero.
    ///
    /// A file per second is what makes a crash survivable — the footage is on disk as it is
    /// recorded — and session mode depends on exactly that. It is also SSD write churn while
    /// the user is merely *waiting* for something worth clipping, which is what
    /// [`EncodeOutput::FragmentedStream`] exists to avoid.
    Segmented,
    /// Fragmented MP4 on the child's **stdout** (`pipe:1`): one `moof`+`mdat` fragment per
    /// forced keyframe, and **no file is written at all**.
    ///
    /// The caller reads that stream and keeps it in memory (`localplay_replay`'s
    /// `MemoryRingBuffer`), so an idle replay buffer touches the disk only when a clip is
    /// actually saved.
    ///
    /// Three things make this work, all of them measured before this was written:
    ///
    /// * `empty_moov+frag_keyframe+default_base_moof` produces a small `ftyp`+`moov` header
    ///   (1249 bytes for a 6s 320x180 capture) followed by one `moof`+`mdat` per fragment, so
    ///   the header can be kept once and any *contiguous range* of fragments appended to it is
    ///   a valid fragmented MP4;
    /// * `frag_keyframe` puts a fragment boundary on every forced keyframe, so `segment_ms`
    ///   stays the cut granularity — the same interval, meaning the same thing, as it does for
    ///   the segmenter;
    /// * each fragment carries a `tfdt` (base media decode time) in the track's timescale
    ///   (15360 for video, 48000 for audio in that measurement), which is monotonic and is how
    ///   a fragment's `[start_ms, end_ms)` is computed without decoding anything.
    FragmentedStream,
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
            // Files on disk, which is the shipping behaviour and the one that survives a
            // crash. A caller that wants no disk churn sets this to `FragmentedStream`.
            output: EncodeOutput::Segmented,
            video: VideoInput::RawPixels,
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
            output: EncodeOutput::Segmented,
            video: VideoInput::RawPixels,
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

    /// Whether this encoder can take a frame whose pixels are a GPU texture rather than bytes.
    ///
    /// The caller uses it to decide whether to ask capture for textures ([`VideoInput`]), and it is
    /// a property of the *configuration* rather than of a running encoder so that the decision can
    /// be made before anything is spawned: the capture backend is told what to deliver while it is
    /// being set up, which is before an encoder exists.
    ///
    /// Defaulted to `false` on the trait, because "no" is the answer for every encoder that is not
    /// this one, and because a mistake in the other direction is the dangerous one: a textured frame
    /// reaching an encoder that wants pixels is refused by name rather than written to a pipe as
    /// nothing (`FfmpegEncoder::submit_video`).
    fn accepts_textures(&self) -> bool {
        false
    }

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
    ///    /// A live capture has no way to slow the world down: when the encoder's queue is
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

    /// Take the encoder's fragmented-MP4 **output stream**, when it was spawned with
    /// [`EncodeOutput::FragmentedStream`].
    ///
    /// `None` for a segmented encoder: its output is files on disk and there is no stream to
    /// read. The default is `None` rather than an error, so "this encoder produces files"
    /// cannot be confused with "somebody already took the stream".
    ///
    /// **Taken, not borrowed.** A pipe has exactly one reader, and the reader is the in-memory
    /// ring buffer — which outlives any borrow it could take from an encoder whose lifetime it
    /// does not control. Taking it makes the ownership explicit and makes a second caller's
    /// `None` honest: there is one stream, and it has gone where it was sent.
    fn take_output_stream(&mut self) -> Option<std::process::ChildStdout> {
        None
    }
}
