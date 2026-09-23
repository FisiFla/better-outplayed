//! Deterministic stand-ins so the whole pipeline is testable off-Windows.

use crate::{AudioBackend, AudioBuffer, AudioFormat, CaptureBackend, Frame, PixelFormat};
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct StubConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Synthetic video: a moving gradient so the encoder sees real inter-frame change.
pub struct StubCapture {
    cfg: StubConfig,
    frame_index: u64,
    started_at: Option<std::time::Instant>,
}

impl StubCapture {
    pub fn new(cfg: StubConfig) -> Self {
        Self { cfg, frame_index: 0, started_at: None }
    }

    /// Produce the frames for `elapsed`, without sleeping.
    pub fn drain_for(&mut self, elapsed: Duration) -> Vec<Frame> {
        let wanted = (elapsed.as_secs_f64() * self.cfg.fps as f64).floor() as u64;
        (0..wanted).map(|_| self.render_next()).collect()
    }

    fn render_next(&mut self) -> Frame {
        let (w, h) = (self.cfg.width, self.cfg.height);
        let phase = (self.frame_index % 256) as u8;
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                data.extend_from_slice(&[
                    (x as u8).wrapping_add(phase), // B
                    (y as u8),                     // G
                    phase,                         // R
                    255,                           // A
                ]);
            }
        }
        let pts = Duration::from_micros(
            self.frame_index * 1_000_000 / self.cfg.fps.max(1) as u64,
        );
        self.frame_index += 1;
        Frame { data, pts, width: w, height: h, format: PixelFormat::Bgra8 }
    }
}

impl CaptureBackend for StubCapture {
    fn start(&mut self) -> anyhow::Result<()> {
        self.frame_index = 0;
        self.started_at = Some(std::time::Instant::now());
        Ok(())
    }

    /// Real-time paced, so the CLI's buffer runs at 1x like a real capture source.
    ///
    /// Returns `None` when the next frame is not due within `timeout`. That is what
    /// lets a single-threaded caller drive video and audio together without either
    /// stream starving (see the CLI loop in Task 13).
    /// `drain_for` deliberately bypasses pacing to keep tests fast.
    fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<Option<Frame>> {
        let started = self
            .started_at
            .ok_or_else(|| anyhow::anyhow!("capture not started"))?;
        let due = Duration::from_micros(
            self.frame_index * 1_000_000 / self.cfg.fps.max(1) as u64,
        );
        let Some(sleep) = due.checked_sub(started.elapsed()) else {
            return Ok(Some(self.render_next())); // already due
        };
        if sleep > timeout {
            return Ok(None);
        }
        std::thread::sleep(sleep);
        Ok(Some(self.render_next()))
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn native_size(&self) -> (u32, u32) {
        (self.cfg.width, self.cfg.height)
    }
}

/// Synthetic audio: silence, so A/V muxing is exercised without needing real audio.
pub struct StubAudio {
    format: AudioFormat,
    block_index: u64,
    started_at: Option<std::time::Instant>,
}

impl StubAudio {
    pub fn new(format: AudioFormat) -> Self {
        Self { format, block_index: 0, started_at: None }
    }

    pub fn drain_for(&mut self, elapsed: Duration) -> Vec<AudioBuffer> {
        let blocks = (elapsed.as_millis() / 10) as u64;
        (0..blocks).map(|_| self.next_block()).collect()
    }

    fn next_block(&mut self) -> AudioBuffer {
        let bytes = self.format.bytes_per_10ms();
        let pts = Duration::from_millis(self.block_index * 10);
        self.block_index += 1;
        AudioBuffer {
            data: vec![0u8; bytes],
            frames: self.format.sample_rate as usize / 100,
            pts,
            format: self.format,
        }
    }
}

impl AudioBackend for StubAudio {
    fn start(&mut self) -> anyhow::Result<()> {
        self.block_index = 0;
        self.started_at = Some(std::time::Instant::now());
        Ok(())
    }

    /// Real-time paced like `StubCapture::next_frame`. Returns `None` when the next
    /// 10ms block is not due within `timeout`; a `Duration::ZERO` timeout makes this
    /// a non-blocking "is anything due?" check for the CLI's drain loop.
    fn next_buffer(&mut self, timeout: Duration) -> anyhow::Result<Option<AudioBuffer>> {
        let started = self
            .started_at
            .ok_or_else(|| anyhow::anyhow!("audio capture not started"))?;
        let due = Duration::from_millis(self.block_index * 10);
        let Some(sleep) = due.checked_sub(started.elapsed()) else {
            return Ok(Some(self.next_block())); // already due
        };
        if sleep > timeout {
            return Ok(None);
        }
        std::thread::sleep(sleep);
        Ok(Some(self.next_block()))
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn stub_emits_the_requested_frame_count_at_the_requested_rate() {
        let cfg = StubConfig { width: 64, height: 48, fps: 60 };
        let mut cap = StubCapture::new(cfg);
        cap.start().unwrap();
        // 0.25s of timeline.
        let frames = cap.drain_for(Duration::from_millis(250));
        assert_eq!(frames.len(), 15, "60fps for 250ms");
    }

    #[test]
    fn stub_frame_pts_are_monotonic_and_frame_sized() {
        let cfg = StubConfig { width: 64, height: 48, fps: 30 };
        let mut cap = StubCapture::new(cfg);
        cap.start().unwrap();
        let frames = cap.drain_for(Duration::from_millis(100));
        assert!(frames.windows(2).all(|w| w[0].pts < w[1].pts), "pts must increase");
        // BGRA8 = 4 bytes per pixel.
        assert_eq!(frames[0].data.len(), 64 * 48 * 4);
    }

    #[test]
    fn stub_audio_emits_silence_in_whole_10ms_blocks() {
        let mut a = StubAudio::new(AudioFormat::default());
        a.start().unwrap();
        let blocks = a.drain_for(Duration::from_millis(50));
        assert_eq!(blocks.len(), 5);
        // 48000Hz / 100 blocks = 480 frames = 960 samples stereo = 1920 bytes.
        assert_eq!(blocks[0].data.len(), 1920);
        assert!(blocks[0].data.iter().all(|b| *b == 0), "stub audio is silence");
    }

    #[test]
    fn next_frame_with_zero_timeout_returns_none_when_not_due() {
        // 10 fps => frames are 100ms apart, a comfortable margin over the handful of
        // microseconds that elapse between the two calls below.
        let cfg = StubConfig { width: 8, height: 8, fps: 10 };
        let mut cap = StubCapture::new(cfg);
        cap.start().unwrap();
        // The initial frame (pts 0) is due immediately.
        assert!(cap.next_frame(Duration::ZERO).unwrap().is_some());
        // The next frame is due 100ms later: a zero timeout must not invent it.
        let started = std::time::Instant::now();
        assert!(
            cap.next_frame(Duration::ZERO).unwrap().is_none(),
            "must return None rather than block or fabricate a frame"
        );
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "a zero timeout must not block waiting for the frame"
        );
    }

    #[test]
    fn next_frame_with_a_generous_timeout_returns_some() {
        let cfg = StubConfig { width: 8, height: 8, fps: 10 };
        let mut cap = StubCapture::new(cfg);
        cap.start().unwrap();
        // Consume the immediately-due frame, then ask for the next one, which is due
        // 100ms out, with a timeout long enough to wait for it.
        assert!(cap.next_frame(Duration::ZERO).unwrap().is_some());
        assert!(
            cap.next_frame(Duration::from_secs(1)).unwrap().is_some(),
            "a generous timeout must sleep until due and return the frame"
        );
    }

    #[test]
    fn next_buffer_with_zero_timeout_returns_none_when_not_due() {
        let mut a = StubAudio::new(AudioFormat::default());
        a.start().unwrap();
        // Block 0 is due immediately.
        assert!(a.next_buffer(Duration::ZERO).unwrap().is_some());
        // Block 1 is due 10ms later: a zero timeout must not block for it.
        assert!(
            a.next_buffer(Duration::ZERO).unwrap().is_none(),
            "must return None rather than block or fabricate a block"
        );
    }
}
