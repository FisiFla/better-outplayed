//! Frame and audio sources.

// Test bodies may unwrap and may chunk by a constant; production code may do neither.
// Scoped to the test build of this crate, so it cannot hide either in shipped code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks))]

use std::time::Duration;

// The waiting machinery below is the Windows capture backend's, and its own tests'. Off Windows
// and outside this crate's test build there is nothing that uses it, so it is not compiled.
#[cfg(any(windows, test))]
use std::sync::{Condvar, Mutex};
#[cfg(any(windows, test))]
use std::time::Instant;

pub mod platform;
pub mod stub;
pub mod target;

pub mod audio_devices;

// The microphone backend is cross-platform so the recorder's microphone path is testable off
// Windows; the real implementation is the `#[cfg(windows)]` half inside it.
pub mod wasapi_mic;

// The synthetic microphone the recorder's microphone path is tested with off Windows. Re-exported
// because tests that wire up a microphone need it and the real backend only exists on Windows.
pub use wasapi_mic::StubMicrophone;

// Cross-platform so its tests run off Windows; only the WASAPI loopback backend uses it.
#[cfg_attr(not(windows), allow(dead_code))]
mod audio_recovery;
#[cfg_attr(not(windows), allow(dead_code))]
mod audio_target;
#[cfg_attr(not(windows), allow(dead_code))]
mod audio_timeline;
pub use audio_target::{GameAudioSource, GameFinder};

// The shape of a display the capture can target, re-exported from the module that holds the rules
// about it so the rest of the app does not have to know where they live.
pub use target::DisplayInfo;

#[cfg(windows)]
pub mod wasapi;
#[cfg(windows)]
pub mod wgc;

/// Every display this machine has, in the order `[capture] monitor` indexes them.
///
/// The primary is first and the rest follow in the order the OS enumerated them, so a display's
/// index here is the index `[capture] monitor` takes for it.
///
/// Empty off Windows: there is no screen to enumerate.
pub fn displays() -> Vec<DisplayInfo> {
    #[cfg(windows)]
    {
        wgc::displays()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// The monotonic clock base shared by the video and audio backends.
///
/// Both take their `pts` from this one `Instant` (QPC on Windows), so A/V alignment is derivable
/// rather than each stream starting a private timeline at its own `start` call.
#[cfg(windows)]
pub(crate) fn clock_base() -> std::time::Instant {
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *BASE.get_or_init(std::time::Instant::now)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Windows Graphics Capture delivers BGRA8.
    Bgra8,
}

/// A captured frame's pixels, still in GPU memory, on a platform that can hand them over.
///
/// Wrapped because the `windows` crate's COM interfaces implement neither `Clone`, `PartialEq`
/// nor `Debug`, and [`Frame`] has all three; a bare `ID3D11Texture2D` field would strip them from
/// every frame on every platform. The impls below spell out the interface's semantics:
///
/// * `Clone` is an `AddRef`, not a pixel copy. It keeps a texture alive after WGC's frame and
///   its pool slot have been released.
/// * `PartialEq` is identity (the same COM object), not "same contents".
/// * `Debug` prints the pointer.
///
/// Off Windows this type is uninhabited, so `Frame::texture` is always `None` there.
#[cfg(windows)]
pub struct GpuTexture(windows::Win32::Graphics::Direct3D11::ID3D11Texture2D);

#[cfg(windows)]
impl GpuTexture {
    /// Wrap a texture the capture backend is handing over.
    pub fn new(texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D) -> Self {
        Self(texture)
    }

    /// The texture itself, for the encoder that binds it to a media buffer.
    pub fn as_raw(&self) -> &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D {
        &self.0
    }
}

#[cfg(windows)]
impl Clone for GpuTexture {
    /// `AddRef`, by way of a `QueryInterface` for the same interface (the `windows` crate's
    /// interfaces are not `Clone`).
    fn clone(&self) -> Self {
        use windows::core::Interface;

        Self(
            self.0
                .cast()
                .expect("a texture's own interface cannot refuse to be AddRef'd"),
        )
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

/// Off Windows no frame keeps its pixels on the GPU, so this type has no values.
#[cfg(not(windows))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuTexture {}

/// A single captured frame.
///
/// The pixels are in [`Frame::data`] unless the backend delivered a GPU texture the encoder asked
/// for: then `data` is empty by design. A consumer that needs bytes must ask
/// ([`Frame::has_pixels`]) rather than treat an empty buffer as a black frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub data: Vec<u8>,
    /// Monotonic, from the same clock as `AudioBuffer::pts`.
    pub pts: Duration,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// The frame's pixels in VRAM, when the backend produced them there and the encoder asked for
    /// that. `None` is the ordinary case, and the only one off Windows.
    pub texture: Option<GpuTexture>,
}

impl Frame {
    /// Whether this frame's pixels are in [`Frame::data`].
    ///
    /// `false` means they are in [`Frame::texture`], not that there are none; a frame with neither
    /// is a bug.
    pub fn has_pixels(&self) -> bool {
        !self.data.is_empty()
    }
}

/// Where a capture backend's time has gone since it started.
///
/// See [`CaptureBackend::frame_timings`] for why the two parts are separated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameTimings {
    /// Time spent with no frame to work on: polling the frame pool and sleeping between polls.
    ///
    /// Not work: it is the display's refresh interval showing up as idle time, and must be subtracted
    /// before any claim about the cost of capture means anything.
    pub waiting: Duration,
    /// Time spent converting a frame: the readback and its copy, or the handover blit.
    pub copying: Duration,
}

// --- Waiting for a frame ---
//
// The Windows capture backend used to ask the frame pool for a frame it did not have, every
// millisecond, until one arrived. That is a COM call into the pool per millisecond per frame
// wanted, and on a 4K desktop it was the largest share of the recording's wall clock. The pool
// itself says when a frame has arrived, so the wait is on that instead.
//
// The machinery is plain concurrency with no platform in it, so it lives here where it can be
// tested off Windows; the backend supplies the producer's half of it.

/// A producer's "something arrived" latch, and the wait a consumer does on it.
///
/// A flag and a condvar. The producer sets the flag and wakes the condvar; the consumer waits on
/// it. The flag is what makes an early report safe: a consumer that reaches the wait after the
/// producer reported finds the flag set and does not wait at all, so a report is never lost
/// between the producer's check and the consumer's wait.
///
/// The producer's half must be cheap and must not block: it runs on a thread of the producer's
/// own, and the consumer may hold the lock for as long as it likes.
#[cfg(any(windows, test))]
pub(crate) struct FrameSignal {
    /// Whether the producer has reported something since the flag was last taken.
    arrived: Mutex<bool>,
    /// Raised with `arrived` whenever the producer reports something.
    wake: Condvar,
}

#[cfg(any(windows, test))]
impl FrameSignal {
    /// A signal with nothing reported yet.
    pub(crate) fn new() -> Self {
        Self {
            arrived: Mutex::new(false),
            wake: Condvar::new(),
        }
    }

    /// Report that the producer has something for the consumer.
    ///
    /// Cheap, and it never waits for the consumer: this takes the lock, sets the flag and lets go.
    pub(crate) fn arrive(&self) {
        let mut arrived = self.arrived.lock().unwrap_or_else(|p| p.into_inner());
        *arrived = true;
        self.wake.notify_all();
    }

    /// Wait up to `timeout` for the producer to report something.
    ///
    /// `true` when the producer reported something — the report is consumed — and `false` when
    /// the wait ran out. A report that landed before this call is not missed: the flag is set
    /// under the same lock this takes, so a report that has already happened is seen here and the
    /// wait returns at once.
    pub(crate) fn wait(&self, timeout: Duration) -> bool {
        let mut arrived = self.arrived.lock().unwrap_or_else(|p| p.into_inner());
        if *arrived {
            *arrived = false;
            return true;
        }
        let (mut arrived, _timed_out) = self
            .wake
            .wait_timeout(arrived, timeout)
            .unwrap_or_else(|p| p.into_inner());
        let reported = *arrived;
        if reported {
            *arrived = false;
        }
        reported
    }
}

/// Take the next thing a producer has, waiting for it to arrive if it has not.
///
/// `take` is asked first: whatever the producer already holds costs nothing to take and no wait
/// at all. When it has nothing, the wait blocks on `signal` until the producer reports that
/// something is there, or `timeout` runs out — and then `take` is asked one last time, because
/// something may have landed while the wait was waking up.
///
/// Returns what `take` produced and how long the wait blocked. The wait is timed as it happened
/// and not as it was asked for: a wait overshoots by the scheduler's granularity, and the
/// overshoot is part of the cost.
#[cfg(any(windows, test))]
pub(crate) fn take_or_wait<T, E>(
    take: &mut dyn FnMut() -> Result<Option<T>, E>,
    signal: &FrameSignal,
    timeout: Duration,
) -> Result<(Option<T>, Duration), E> {
    let deadline = Instant::now() + timeout;
    let mut waited = Duration::ZERO;
    loop {
        if let Some(found) = take()? {
            return Ok((Some(found), waited));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok((None, waited));
        }
        let before = Instant::now();
        let reported = signal.wait(remaining);
        waited += before.elapsed();
        if !reported {
            // The wait ran out. One last look: something may have landed as we woke up.
            return take().map(|found| (found, waited));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl Default for AudioFormat {
    fn default() -> Self {
        // Fixed at 48kHz stereo so the audio timeline is exactly derivable from the byte count.
        Self {
            sample_rate: 48_000,
            channels: 2,
        }
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
/// Reported, never converted by hand: the WASAPI backend asks for 48kHz stereo s16le whatever the
/// endpoint uses and the audio engine converts (`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`). This type
/// lets the log say what the engine converts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleEncoding {
    F32,
    I16,
    I32,
    Other,
}

/// An endpoint's native (mix) format, as the capture backends found it.
///
/// Log evidence only: the pipeline runs on [`AudioFormat::default`], so nothing downstream sees
/// one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeAudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub encoding: SampleEncoding,
}

impl NativeAudioFormat {
    /// Whether the endpoint is not already the canonical 48kHz stereo s16le, i.e. the engine's
    /// converter has work to do. Only labels the log line; the stream is opened with
    /// auto-conversion either way.
    pub fn needs_conversion(&self) -> bool {
        let canonical = AudioFormat::default();
        self.sample_rate != canonical.sample_rate
            || self.channels != canonical.channels
            || self.encoding != SampleEncoding::I16
    }
}

/// How long an unbroken run of silence must last, on a sample-rate-converted endpoint, before
/// the absence of audio is worth a warning.
///
/// A judgement call, not a measurement. On some Windows 11 builds a loopback stream opened with
/// `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` has been reported to deliver only silence (unverified
/// here), and samples cannot tell a broken converter from a muted game or paused desktop.
/// Thirty seconds is long enough that a game's first sound clears the watch and ordinary
/// transients (splash screens, a mute toggle) cannot accumulate to it, since any real audio
/// resets the run, yet short enough to name a broken converter in the first half-minute. It warns
/// rather than fails because a legitimately silent machine would trip it.
pub const CONVERTED_SILENCE_THRESHOLD: Duration = Duration::from_secs(30);

/// Watches a capture session for the reported failure where an auto-converted WASAPI loopback
/// stream delivers only silence.
///
/// Pure and platform-independent so the decision is unit-testable off Windows; the WASAPI backend
/// feeds it each 10ms block. It raises at most one warning per session, and only when the endpoint
/// needed engine-side conversion (otherwise silence is just silence), the audio is one unbroken
/// silent run of at least [`CONVERTED_SILENCE_THRESHOLD`], and no real audio was ever observed
/// (one real sample proves the converter works and disarms the warning).
#[derive(Debug, Clone)]
pub struct ConvertedSilenceWatch {
    /// Whether the audio engine was doing sample-rate conversion for this session.
    converting: bool,
    /// Whether the single warning has already been raised.
    warned: bool,
    /// Whether any real (non-silent) audio has been observed.
    proven_audible: bool,
    /// Total audio observed, over the whole session.
    elapsed: Duration,
    /// Length of the unbroken run of silence in progress.
    silent_run: Duration,
}

impl ConvertedSilenceWatch {
    /// A watch for a session whose endpoint did (`converting`) or did not need engine-side conversion.
    pub fn new(converting: bool) -> Self {
        Self {
            converting,
            warned: false,
            proven_audible: false,
            elapsed: Duration::ZERO,
            silent_run: Duration::ZERO,
        }
    }

    /// Observe one block of length `block`; returns `true` if the warning should be raised now.
    ///
    /// Returns `true` at most once: the first call that sees unbroken silence cross the threshold,
    /// with conversion in play and no audio ever heard, latches. A block with any real audio resets
    /// the run and disarms the watch permanently.
    pub fn observe(&mut self, block: Duration, silent: bool) -> bool {
        self.elapsed += block;
        if !silent {
            // Real audio proves conversion works; later silence is just silence. Disarm for good.
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

    /// Total audio observed so far.
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// Length of the unbroken run of silence in progress.
    pub fn silent_run(&self) -> Duration {
        self.silent_run
    }
}

/// Whether a block of interleaved s16le audio is entirely silent (every sample zero).
///
/// The engine's silent flag is turned into explicit zeros before a block reaches this (see the
/// WASAPI backend's `append_packet`). An empty slice is vacuously silent.
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

    /// Close any frames the backend has buffered without copying their pixels to CPU memory, and
    /// return how many were discarded. Lets the caller skip the readback for frames the pacer will
    /// not keep (at 3840x2160 BGRA that is a 33.2MB GPU copy plus a row-by-row CPU copy per frame).
    ///
    /// Implementations must do no allocation, no pixel copy and no blocking wait, must release what
    /// they close (an unclosed frame occupies a backend buffer and starves the next one), and must
    /// bound their loop. Counting contract: a frame is either returned by
    /// [`CaptureBackend::next_frame`] or counted here, so the caller's counters account for every
    /// frame offered.
    ///
    /// The default suits a backend with no frame pool (nothing to discard); the stub overrides it to
    /// model a paced source and WGC to close the frames its pool holds.
    fn discard_pending(&mut self) -> anyhow::Result<usize> {
        Ok(0)
    }

    /// How this backend has spent its time since it started: waiting for frames, and copying them.
    ///
    /// `next_frame` is one number, which cannot say whether the readback is the cost: a backend
    /// waiting on a 60 Hz display reports the same figure as one copying 33 MB out of VRAM, and the
    /// remedies are opposite. `None` for a backend that does not measure it; one that does must
    /// account for all the time spent inside `next_frame` (waiting plus copying).
    fn frame_timings(&self) -> Option<FrameTimings> {
        None
    }

    /// Ask this backend to hand frames over as GPU textures instead of copying their pixels to
    /// [`Frame::data`].
    ///
    /// Off by default; a backend that cannot do it ignores the call and keeps delivering pixels. Only
    /// a backend whose frames are born on the GPU saves anything (WGC, where the copy out of VRAM is
    /// ~97% of the 4K pipeline's cost). The caller needs an encoder that can take a texture (a
    /// hardware MFT through an `IMFDXGIDeviceManager`) on the same D3D11 device; without one, frames
    /// carry no pixels and the encoder refuses them by name rather than writing a zero-length frame.
    fn deliver_textures(&mut self, _textures: bool) {}

    /// The frame geometry this backend delivers.
    ///
    /// Infallible: the size is fixed at construction (the monitor's native resolution for WGC, the
    /// configured size for the stub). The encoder declares its rawvideo pipe with exactly this size.
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
        NativeAudioFormat {
            sample_rate,
            channels,
            encoding,
        }
    }

    #[test]
    fn an_endpoint_that_is_already_canonical_needs_no_conversion() {
        assert!(!native(48_000, 2, SampleEncoding::I16).needs_conversion());
    }

    #[test]
    fn any_other_rate_channel_count_or_encoding_is_flagged_for_engine_conversion() {
        // 44.1kHz USB DACs and 96kHz wireless headsets are what engine-side conversion is for.
        assert!(native(44_100, 2, SampleEncoding::I16).needs_conversion());
        assert!(native(96_000, 2, SampleEncoding::I16).needs_conversion());
        // The other two axes the engine converts.
        assert!(native(48_000, 6, SampleEncoding::I16).needs_conversion());
        assert!(native(48_000, 1, SampleEncoding::I16).needs_conversion());
        assert!(native(48_000, 2, SampleEncoding::F32).needs_conversion());
        assert!(native(48_000, 2, SampleEncoding::I32).needs_conversion());
        assert!(native(48_000, 2, SampleEncoding::Other).needs_conversion());
    }

    // --- Converted-silence watch ---
    // These pin the decision, not the externally reported failure; pure logic, so they run off Windows.

    /// Seconds, to match the units the threshold is documented in.
    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn short_silence_does_not_warn() {
        // One second short of the threshold: a silent splash screen is not a broken converter.
        let mut watch = ConvertedSilenceWatch::new(true);
        for _ in 0..CONVERTED_SILENCE_THRESHOLD.as_secs() - 1 {
            assert!(
                !watch.observe(secs(1), true),
                "under the threshold: no warning"
            );
        }
        assert!(!watch.has_warned());
    }

    #[test]
    fn sustained_silence_with_conversion_warns_exactly_once() {
        // A converted endpoint that never delivers audio: 40s of 10ms blocks, past the 30s threshold.
        let mut watch = ConvertedSilenceWatch::new(true);
        let mut warnings = 0;
        for _ in 0..4_000 {
            if watch.observe(Duration::from_millis(10), true) {
                warnings += 1;
            }
        }
        assert_eq!(
            warnings, 1,
            "warn once per session, not once per 10ms block"
        );
        assert!(watch.has_warned());
    }

    #[test]
    fn the_warning_fires_at_the_threshold_and_not_before() {
        // The boundary, block by block: 30s of one-second blocks.
        let mut watch = ConvertedSilenceWatch::new(true);
        for second in 0..CONVERTED_SILENCE_THRESHOLD.as_secs() {
            let warned = watch.observe(secs(1), true);
            if second + 1 < CONVERTED_SILENCE_THRESHOLD.as_secs() {
                assert!(
                    !warned,
                    "second {} is still under the threshold",
                    second + 1
                );
            } else {
                assert!(warned, "the crossing second must warn");
            }
        }
    }

    #[test]
    fn sustained_silence_without_conversion_never_warns() {
        // Already 48kHz stereo s16: no conversion to blame, so a muted game is never flagged.
        let mut watch = ConvertedSilenceWatch::new(false);
        for _ in 0..10_000 {
            assert!(
                !watch.observe(Duration::from_millis(10), true),
                "nothing to blame"
            );
        }
        assert!(!watch.has_warned());
    }

    #[test]
    fn silence_broken_by_real_audio_resets_and_never_warns() {
        // Silence under the threshold, one real block, then silence well over it: real audio resets and
        // permanently disarms the watch, so the later silence is a mute.
        let mut watch = ConvertedSilenceWatch::new(true);
        for _ in 0..20 {
            assert!(!watch.observe(secs(1), true));
        }
        assert_eq!(watch.silent_run(), secs(20), "the run accumulated");
        assert!(!watch.observe(secs(1), false), "real audio must never warn");
        assert_eq!(
            watch.silent_run(),
            Duration::ZERO,
            "the run resets on real audio"
        );
        for _ in 0..120 {
            assert!(
                !watch.observe(secs(1), true),
                "audio was heard; silence is now benign"
            );
        }
        assert!(!watch.has_warned());
    }

    #[test]
    fn the_watch_counts_both_total_elapsed_and_the_current_silent_run() {
        // Total time observed and the unbroken silent run within it are kept separately.
        let mut watch = ConvertedSilenceWatch::new(true);
        watch.observe(secs(5), false); // real audio
        watch.observe(secs(3), true); //  silence begins
        assert_eq!(watch.elapsed(), secs(8), "elapsed spans the whole session");
        assert_eq!(
            watch.silent_run(),
            secs(3),
            "only the trailing run is silent"
        );
        assert!(!watch.has_warned());
    }

    #[test]
    fn is_silent_block_is_true_only_for_an_all_zero_block() {
        assert!(is_silent_block(&[0, 0, 0, 0]));
        // A single non-zero byte anywhere is real audio, even -1 (0xFFFF) or +1.
        assert!(!is_silent_block(&[0, 0, 1, 0]));
        assert!(!is_silent_block(&[0xFF, 0xFF, 0, 0]));
        assert!(!is_silent_block(&[0, 0, 0, 0x80]));
    }

    #[test]
    fn off_windows_there_is_no_screen_to_enumerate() {
        if cfg!(not(windows)) {
            assert!(displays().is_empty());
        }
    }

    /// The signal exists so a report that lands before the consumer waits is not missed. Miss
    /// it and the consumer waits a whole timeout for a frame that is already there.
    #[test]
    fn a_report_that_landed_before_the_wait_is_not_missed() {
        let signal = FrameSignal::new();
        // The producer reports first. The consumer's first ask finds nothing — the frame is not
        // there yet — but the report was.
        signal.arrive();
        let mut asks = 0u32;
        let mut take = || {
            asks += 1;
            Ok::<Option<u32>, ()>(if asks >= 2 { Some(7) } else { None })
        };
        let (found, waited) = take_or_wait(&mut take, &signal, Duration::from_secs(5))
            .expect("the take does not fail");
        assert_eq!(found, Some(7), "the frame the report announced was taken");
        assert!(
            waited < Duration::from_secs(1),
            "the consumer waited a real timeout for a report that had already landed: {waited:?}"
        );
    }

    /// A wait that finds nothing must give up inside its budget, and must report the wait it
    /// actually did rather than the one it asked for.
    #[test]
    fn a_wait_that_finds_nothing_gives_up_inside_its_budget() {
        let signal = FrameSignal::new();
        let mut take = || Ok::<Option<u32>, ()>(None);
        let before = Instant::now();
        let (found, waited) = take_or_wait(&mut take, &signal, Duration::from_millis(50))
            .expect("the take does not fail");
        let elapsed = before.elapsed();
        assert_eq!(found, None, "there was nothing to take");
        assert!(
            elapsed < Duration::from_secs(2),
            "the wait ran past its budget: {elapsed:?}"
        );
        assert!(
            waited > Duration::ZERO,
            "the wait blocked, and says how long it blocked"
        );
    }

    /// A frame the producer already holds costs no wait at all. This is the case the old
    /// polling loop paid for with a millisecond of sleep on every single frame.
    #[test]
    fn a_frame_that_is_already_there_costs_no_wait() {
        let signal = FrameSignal::new();
        let mut take = || Ok::<Option<u32>, ()>(Some(42));
        let (found, waited) = take_or_wait(&mut take, &signal, Duration::from_secs(5))
            .expect("the take does not fail");
        assert_eq!(found, Some(42));
        assert!(
            waited < Duration::from_millis(50),
            "a frame that was already there should not have waited: {waited:?}"
        );
    }
}
