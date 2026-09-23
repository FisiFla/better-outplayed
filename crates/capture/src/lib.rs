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
