//! Frame and audio sources.

use std::time::Duration;

pub mod platform;
pub mod stub;

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

/// A single captured frame with CPU-visible pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub data: Vec<u8>,
    /// Monotonic, from the same clock as `AudioBuffer::pts` (spec §5.1).
    pub pts: Duration,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
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
}
