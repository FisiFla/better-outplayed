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
}

pub trait AudioBackend: Send {
    fn start(&mut self) -> anyhow::Result<()>;
    fn next_buffer(&mut self, timeout: Duration) -> anyhow::Result<Option<AudioBuffer>>;
    fn stop(&mut self) -> anyhow::Result<()>;
}
