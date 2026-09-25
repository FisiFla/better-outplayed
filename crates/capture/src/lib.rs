//! Frame and audio sources.

use std::time::Duration;

pub mod platform;
pub mod stub;

// The microphone backend: a cross-platform module (so the recorder's microphone path is
// testable off Windows) whose real implementation is the `#[cfg(windows)]` half inside it.
pub mod wasapi_mic;

// The synthetic microphone the recorder's microphone path is tested with off Windows (see
// `wasapi_mic::StubMicrophone`). Re-exported because it is a first-class part of the crate's
// public surface: a test that wires up a microphone needs it, and the real backend only
// exists on Windows.
pub use wasapi_mic::StubMicrophone;

#[cfg(windows)]
pub mod wasapi;
#[cfg(windows)]
pub mod wgc;

/// The monotonic clock base shared by the video and audio backends.
///
/// Both backends take their `pts` from this one `Instant`, so A/V alignment is
/// derivable: the two streams are measured from the same origin (spec §5.1) rather
/// than each starting a private timeline at its own `start` call. `Instant` is the
/// QPC clock on Windows, which is what the spec means by "QPC-based".
#[cfg(windows)]
pub(crate) fn clock_base() -> std::time::Instant {
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *BASE.get_or_init(std::time::Instant::now)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Windows Graphics Capture hands us BGRA8.
    Bgra8,
}

/// A captured frame's pixels, still in GPU memory, on a platform that can hand them over.
///
/// Wrapped rather than used bare because the `windows` crate's COM interfaces implement neither
/// `Clone`, `PartialEq` nor `Debug`, and [`Frame`] has all three. A bare `ID3D11Texture2D` field
/// would therefore strip three derives off every frame on every platform, which is a large blast
/// radius for one platform's optimisation. The three impls below are the interface's own
/// semantics, spelled out once, here:
///
/// * **`Clone` is an `AddRef`**, not a copy of pixels. It is what keeps a texture alive after
///   WGC's frame — and the pool slot behind it — has been released, which is the lifetime
///   question this whole path turns on.
/// * **`PartialEq` is identity**: equal when they are the same COM object. A frame has never
///   meant "same contents" and comparing 33.2MB to answer that would be absurd.
/// * **`Debug` prints the pointer**, because the interface has no `Debug` of its own and a
///   frame's log line should not need one.
///
/// Off Windows this type is **uninhabited**, so `Frame::texture` can only ever be `None` there —
/// which is the honest statement rather than a placeholder: every other capture backend copies
/// its pixels into `Frame::data`, so there is nothing to hand over.
#[cfg(windows)]
pub struct GpuTexture(windows::Win32::Graphics::Direct3D11::ID3D11Texture2D);

#[cfg(windows)]
impl GpuTexture {
    /// Wrap a texture the capture backend is handing over.
    pub fn new(texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D) -> Self {
        Self(texture)
    }

    /// The texture itself, for the encoder that will bind it to a media buffer.
    pub fn as_raw(&self) -> &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D {
        &self.0
    }
}

#[cfg(windows)]
impl Clone for GpuTexture {
    /// `AddRef`, by way of a `QueryInterface` for the same interface — the `windows` crate's
    /// interfaces are not `Clone`, and re-querying is its own safe idiom for taking a reference.
    fn clone(&self) -> Self {
        use windows::core::Interface;

        Self(self.0.cast().expect("a texture's own interface cannot refuse to be AddRef'd"))
    }
}

#[cfg(windows)]
impl PartialEq for GpuTexture {
    fn eq(&self, other: &Self) -> bool {
        use windows::core::Interface;

        self.0.as_raw() == other.0.as_raw()
    }
}

#[cfg(windows)]
impl Eq for GpuTexture {}

#[cfg(windows)]
impl std::fmt::Debug for GpuTexture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use windows::core::Interface;

        f.debug_tuple("GpuTexture").field(&self.0.as_raw()).finish()
    }
}

/// See the Windows variant above: off Windows there is no such thing as a frame whose pixels
/// never reached the CPU, so this type has no values at all.
#[cfg(not(windows))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuTexture {}

/// A single captured frame.
///
/// The pixels are in [`Frame::data`] **unless** the backend delivered them as a GPU texture and
/// the encoder asked for that: in that case `data` is empty *by design*, because the pixels never
/// left VRAM. A consumer that needs bytes must therefore ask ([`Frame::has_pixels`]) rather than
/// finding an empty buffer and treating it as a black frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub data: Vec<u8>,
    /// Monotonic, from the same clock as `AudioBuffer::pts` (spec §5.1).
    pub pts: Duration,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// The frame's pixels in VRAM, when the backend produced them there and the encoder asked
    /// for them that way. `None` is the ordinary case and the only case off Windows.
    pub texture: Option<GpuTexture>,
}

impl Frame {
    /// Whether this frame's pixels are in [`Frame::data`].
    ///
    /// `false` means the pixels are in [`Frame::texture`], not that there are none: the two are
    /// alternatives, and a frame with neither is a bug rather than a black picture.
    pub fn has_pixels(&self) -> bool {
        !self.data.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl Default for AudioFormat {
    fn default() -> Self {
        // Fixed at 48kHz stereo: the audio timeline must be exactly derivable
        // from the byte count (spec §13).
        Self { sample_rate: 48_000, channels: 2 }
    }
}

impl AudioFormat {
    /// Bytes per 10ms block, the granularity WASAPI loopback delivers.
    pub fn bytes_per_10ms(&self) -> usize {
        (self.sample_rate as usize / 100) * self.channels as usize * 2
    }
}

/// The sample encoding an endpoint's native (mix) format uses.
///
/// Reported, never converted by hand: the WASAPI backend opens its stream asking for
/// 48kHz stereo s16le whatever the endpoint natively uses, and the audio engine's sample
/// rate converter does the work (`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`). This type exists
/// so the capture log can say what the engine is converting *from*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleEncoding {
    /// 32-bit float in [-1, 1].
    F32,
    /// 16-bit signed PCM.
    I16,
    /// 32-bit signed PCM.
    I32,
    /// Anything else (8-bit PCM, 64-bit float, an unrecognised SubFormat GUID).
    Other,
}

/// An endpoint's native (mix) format, as the capture backends found it.
///
/// Log evidence only: the canonical format the pipeline runs on is
/// [`AudioFormat::default`] and the backend always asks the engine for that, so nothing
/// downstream ever sees one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeAudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub encoding: SampleEncoding,
}

impl NativeAudioFormat {
    /// Whether the endpoint is *not* already the canonical 48kHz stereo s16le, i.e. the
    /// audio engine's converter has work to do.
    ///
    /// This changes no behaviour — the stream is opened with auto-conversion either way
    /// — it only labels the log line. A `true` here together with audible, correctly
    /// sized audio in the produced clip is what shows the engine-side conversion is
    /// really happening.
    pub fn needs_conversion(&self) -> bool {
        let canonical = AudioFormat::default();
        self.sample_rate != canonical.sample_rate
            || self.channels != canonical.channels
            || self.encoding != SampleEncoding::I16
    }
}

/// How long an unbroken run of silence must last before the absence of audio is worth a
/// warning, when the endpoint is being sample-rate-converted.
///
/// This is a judgement call, not a measurement. The failure mode being looked for is
/// external and **unverified at runtime**: on some Windows 11 builds a loopback capture
/// stream opened with `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` has been reported to deliver
/// only silence (Rust/cpal and screenpipe users). We cannot tell "the converter delivered
/// nothing" apart from "the machine was genuinely quiet" by looking at samples: a muted
/// game, a paused desktop and a broken converter all yield the same all-zero stream.
///
/// Thirty seconds is chosen to be:
/// - **long enough** that a clip whose game does produce sound will have produced at least
///   one non-silent sample and cleared the watch (games play their first SFX within a
///   second or two of launching), and that ordinary transients — a silent splash or
///   loading screen, the gap before the first sound, a brief mute toggle — cannot
///   accumulate to it, because the run resets the moment any real audio arrives;
/// - **short enough** that a broken converter is named in the first half-minute of a clip
///   rather than after a whole session; and
/// - a round number a human reading the log can reason about.
///
/// The residual false positive — a machine that is legitimately silent (muted, idle) for
/// the whole window — is exactly why this warns and neither fails nor stops the capture.
pub const CONVERTED_SILENCE_THRESHOLD: Duration = Duration::from_secs(30);

/// Watches a capture session for the reported failure mode where an auto-converted WASAPI
/// loopback stream delivers only silence.
///
/// Pure and platform-independent on purpose: it consumes a stream of `(duration, silent)`
/// observations and decides whether to raise the warning, so the decision is unit-testable
/// on a host with no audio APIs. The WASAPI backend owns one and feeds it each 10ms block;
/// nothing here touches WASAPI.
///
/// The watch raises **at most one** warning per session, and only when *all* of these hold:
/// - the endpoint needed engine-side conversion (`converting`): if it was already 48kHz
///   stereo s16 there is no conversion to blame, and silence is just silence;
/// - the audio is a single unbroken run of silence at least [`CONVERTED_SILENCE_THRESHOLD`]
///   long; and
/// - no real (non-silent) audio has ever been observed — one real sample proves the
///   converter is delivering audio, which permanently disarms the warning.
#[derive(Debug, Clone)]
pub struct ConvertedSilenceWatch {
    /// Whether the audio engine was doing sample-rate conversion for this session.
    converting: bool,
    /// Whether the single warning has already been raised.
    warned: bool,
    /// Whether any real (non-silent) audio has been observed: if so, conversion works.
    proven_audible: bool,
    /// Total audio observed, over the whole session.
    elapsed: Duration,
    /// Length of the unbroken run of silence currently in progress.
    silent_run: Duration,
}

impl ConvertedSilenceWatch {
    /// A watch for a session whose endpoint did (`converting = true`) or did not require
    /// engine-side conversion.
    pub fn new(converting: bool) -> Self {
        Self {
            converting,
            warned: false,
            proven_audible: false,
            elapsed: Duration::ZERO,
            silent_run: Duration::ZERO,
        }
    }

    /// Observe one block of audio of length `block`, and report whether the warning should
    /// be raised *now*.
    ///
    /// Returns `true` at most once in the watch's lifetime: the first call that sees the
    /// unbroken silence cross the threshold — with conversion in play and no audio ever
    /// heard — returns `true` and latches, and every later call returns `false`. A block
    /// containing any real audio resets the silence run to zero and disarms the watch
    /// permanently (a non-silent sample falsifies the hypothesis it exists to test).
    pub fn observe(&mut self, block: Duration, silent: bool) -> bool {
        self.elapsed += block;
        if !silent {
            // Real audio proves the conversion is working; from here on silence is just
            // silence and the warning would be a false positive. Clear it for good.
            self.silent_run = Duration::ZERO;
            self.proven_audible = true;
            return false;
        }
        self.silent_run += block;
        if self.converting
            && !self.proven_audible
            && !self.warned
            && self.silent_run >= CONVERTED_SILENCE_THRESHOLD
        {
            self.warned = true;
            return true;
        }
        false
    }

    /// Whether this watch has already raised its warning.
    pub fn has_warned(&self) -> bool {
        self.warned
    }

    /// Total audio observed so far, over the whole session.
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// Length of the unbroken run of silence currently in progress.
    pub fn silent_run(&self) -> Duration {
        self.silent_run
    }
}

/// Whether a block of interleaved s16le audio is entirely silent — every sample zero.
///
/// The engine's own silent flag is turned into explicit zeros before a block reaches this
/// (see the WASAPI backend's `append_packet`), so an all-zero block is the single
/// representation of "no audio" the pipeline has. An empty slice is vacuously silent; real
/// blocks are never empty.
pub fn is_silent_block(samples: &[u8]) -> bool {
    samples.iter().all(|&byte| byte == 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioBuffer {
    /// Interleaved PCM, s16le.
    pub data: Vec<u8>,
    pub frames: usize,
    pub pts: Duration,
    pub format: AudioFormat,
}

pub trait CaptureBackend: Send {
    fn start(&mut self) -> anyhow::Result<()>;
    fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<Option<Frame>>;
    fn stop(&mut self) -> anyhow::Result<()>;

    /// Close any frames the backend has buffered, without copying their pixels to CPU
    /// memory. Returns how many were discarded. Used to skip the (expensive) readback
    /// for frames the pacer will not keep.
    ///
    /// This is the cheap half of [`CaptureBackend::next_frame`], and the caller decides
    /// which half to use *before* paying for a frame: the rate-limiting pacer in the CLI
    /// discards the frames it has no slot for (see `pump_once_counted`), and at
    /// 3840x2160 BGRA the readback it skips is a 33.2MB GPU copy plus a row-by-row CPU
    /// copy per frame. Implementations must therefore do no allocation, no pixel copy and
    /// no blocking wait here, must release what they close (an unclosed frame occupies a
    /// backend buffer and starves the next one), and must bound their loop.
    ///
    /// Counting contract: a frame is either returned by [`CaptureBackend::next_frame`] or
    /// counted here, so the caller's materialised and discarded counters together account
    /// for every frame the backend offered.
    ///
    /// The default is correct for a backend that owns no frame pool — there is nothing
    /// buffered to close, so there is nothing to discard. The stub overrides it to model a
    /// paced source, and WGC overrides it to close the frames its pool is holding.
    fn discard_pending(&mut self) -> anyhow::Result<usize> {
        Ok(0)
    }

    /// Ask this backend to hand frames over as GPU textures instead of copying their pixels to
    /// [`Frame::data`].
    ///
    /// Off by default, and a backend that cannot do it says so by leaving it off: the default is a
    /// no-op, and a backend that ignores it keeps delivering pixels, which every consumer
    /// understands. Only a backend whose frames are *born* on the GPU can save anything here — on
    /// this project that is Windows Graphics Capture, where the copy out of VRAM is ~97% of what
    /// the 4K pipeline does (§14 of `docs/verification-status.md`).
    ///
    /// Textures are only useful to an encoder that can *take* one (a hardware MFT through a
    /// `IMFDXGIDeviceManager`), and that encoder must be on the same D3D11 device. A caller that
    /// turns this on without one gets frames with no pixels in them and, in this project, an
    /// encoder that refuses them by name rather than writing a zero-length frame into a pipe.
    fn deliver_textures(&mut self, _textures: bool) {}

    /// The frame geometry this backend delivers.
    ///
    /// Infallible by design: the size is fixed once the backend is constructed (the
    /// monitor's native resolution for WGC, the configured size for the stub), so
    /// there is no state that can be "not started yet" to report around. The encoder
    /// declares its rawvideo pipe with exactly this size, so the two never disagree.
    fn native_size(&self) -> (u32, u32);
}

pub trait AudioBackend: Send {
    fn start(&mut self) -> anyhow::Result<()>;
    fn next_buffer(&mut self, timeout: Duration) -> anyhow::Result<Option<AudioBuffer>>;
    fn stop(&mut self) -> anyhow::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native(sample_rate: u32, channels: u16, encoding: SampleEncoding) -> NativeAudioFormat {
        NativeAudioFormat { sample_rate, channels, encoding }
    }

    #[test]
    fn the_canonical_audio_format_is_48khz_stereo_s16() {
        // The format the WASAPI backend asks the audio engine for, and the one the
        // encoder's audio pipe is declared as (`-f s16le -ar 48000 -ac 2`).
        let canonical = AudioFormat::default();
        assert_eq!(canonical.sample_rate, 48_000);
        assert_eq!(canonical.channels, 2);
        // 10ms of it: 480 frames x 2 channels x 2 bytes. This is the block `next_buffer`
        // slices out and the granularity the rest of the pipeline counts in.
        assert_eq!(canonical.bytes_per_10ms(), 1_920);
    }

    #[test]
    fn an_endpoint_that_is_already_canonical_needs_no_conversion() {
        assert!(!native(48_000, 2, SampleEncoding::I16).needs_conversion());
    }

    #[test]
    fn any_other_rate_channel_count_or_encoding_is_flagged_for_engine_conversion() {
        // The rates that used to make `start()` refuse: 44.1kHz USB DACs and 96kHz
        // wireless headsets are exactly what the engine-side conversion is for.
        assert!(native(44_100, 2, SampleEncoding::I16).needs_conversion());
        assert!(native(96_000, 2, SampleEncoding::I16).needs_conversion());
        // The two other axes the engine also converts for us.
        assert!(native(48_000, 6, SampleEncoding::I16).needs_conversion());
        assert!(native(48_000, 1, SampleEncoding::I16).needs_conversion());
        assert!(native(48_000, 2, SampleEncoding::F32).needs_conversion());
        assert!(native(48_000, 2, SampleEncoding::I32).needs_conversion());
        assert!(native(48_000, 2, SampleEncoding::Other).needs_conversion());
    }

    // --- Converted-silence watch -------------------------------------------------------
    //
    // The failure mode is externally reported and unverified at runtime; these tests pin
    // the *decision*, not the report. They run off Windows because the logic is pure.

    /// Seconds, so the tests read in the units the threshold is documented in.
    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn short_silence_does_not_warn() {
        // One second short of the threshold, in 1s blocks: a silent splash screen or the gap
        // before a game's first sound must not be mistaken for a broken converter.
        let mut watch = ConvertedSilenceWatch::new(true);
        for _ in 0..CONVERTED_SILENCE_THRESHOLD.as_secs() - 1 {
            assert!(!watch.observe(secs(1), true), "under the threshold: no warning");
        }
        assert!(!watch.has_warned());
    }

    #[test]
    fn sustained_silence_with_conversion_warns_exactly_once() {
        // The reported failure mode: a converted endpoint that never delivers audio.
        // 4000 blocks of 10ms is 40s — well past the 30s threshold.
        let mut watch = ConvertedSilenceWatch::new(true);
        let mut warnings = 0;
        for _ in 0..4_000 {
            if watch.observe(Duration::from_millis(10), true) {
                warnings += 1;
            }
        }
        assert_eq!(warnings, 1, "warn once per session, not once per 10ms block");
        assert!(watch.has_warned());
    }

    #[test]
    fn the_warning_fires_at_the_threshold_and_not_before() {
        // The boundary, block by block: 30s of one-second blocks.
        let mut watch = ConvertedSilenceWatch::new(true);
        for second in 0..CONVERTED_SILENCE_THRESHOLD.as_secs() {
            let warned = watch.observe(secs(1), true);
            if second + 1 < CONVERTED_SILENCE_THRESHOLD.as_secs() {
                assert!(!warned, "second {} is still under the threshold", second + 1);
            } else {
                assert!(warned, "the crossing second must warn");
            }
        }
    }

    #[test]
    fn sustained_silence_without_conversion_never_warns() {
        // An endpoint already at 48kHz stereo s16: silence is just silence. There is no
        // conversion to blame, so a muted game or an idle desktop must never be flagged.
        let mut watch = ConvertedSilenceWatch::new(false);
        for _ in 0..10_000 {
            assert!(!watch.observe(Duration::from_millis(10), true), "nothing to blame");
        }
        assert!(!watch.has_warned());
    }

    #[test]
    fn silence_broken_by_real_audio_resets_and_never_warns() {
        // A run of silence UNDER the threshold, one block of real audio, then a run of
        // silence WELL OVER it. Real audio proves the converter works, so the watch is both
        // reset and permanently disarmed: the later silence is a mute, not the failure mode.
        let mut watch = ConvertedSilenceWatch::new(true);
        for _ in 0..20 {
            assert!(!watch.observe(secs(1), true));
        }
        assert_eq!(watch.silent_run(), secs(20), "the run accumulated");
        assert!(!watch.observe(secs(1), false), "real audio must never warn");
        assert_eq!(watch.silent_run(), Duration::ZERO, "the run resets on real audio");
        for _ in 0..120 {
            assert!(!watch.observe(secs(1), true), "audio was heard; silence is now benign");
        }
        assert!(!watch.has_warned());
    }

    #[test]
    fn the_watch_counts_both_total_elapsed_and_the_current_silent_run() {
        // The two durations the decision is made from, kept separately: total time observed
        // and the unbroken silent run within it.
        let mut watch = ConvertedSilenceWatch::new(true);
        watch.observe(secs(5), false); // real audio
        watch.observe(secs(3), true); //  silence begins
        assert_eq!(watch.elapsed(), secs(8), "elapsed spans the whole session");
        assert_eq!(watch.silent_run(), secs(3), "only the trailing run is silent");
        assert!(!watch.has_warned());
    }

    #[test]
    fn is_silent_block_is_true_only_for_an_all_zero_block() {
        assert!(is_silent_block(&[0, 0, 0, 0]));
        // A single non-zero byte anywhere is real audio — even a sample of -1 (0xFFFF) or +1.
        assert!(!is_silent_block(&[0, 0, 1, 0]));
        assert!(!is_silent_block(&[0xFF, 0xFF, 0, 0]));
        assert!(!is_silent_block(&[0, 0, 0, 0x80]));
    }
}
