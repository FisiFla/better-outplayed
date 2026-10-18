//! Keeps the game-audio timeline running across a source that goes away or should be swapped.
//!
//! Unplugging a headset invalidates the loopback stream, and switching the default output leaves
//! it capturing a device nobody hears. Either way the stream is reopened on whatever is wanted now,
//! and the [`AudioTimeline`] is never reset: the outage is padded with silence like any other gap,
//! so everything after it still lines up with the video.

use crate::audio_timeline::{AudioTimeline, Block, PacketSource};
use anyhow::Result;
use std::time::Duration;

/// How often a lost source is reopened. Short enough that a headset swap loses well under a
/// second, long enough that a machine with no output device at all is not hammered.
pub(crate) const RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// How often the wanted source is compared with the open one.
pub(crate) const SWITCH_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Packets drained per poll, so a chatty source cannot spin the drain loop forever.
const MAX_PACKETS_PER_POLL: usize = 16;

/// The open stream's endpoint has been removed or invalidated (`AUDCLNT_E_DEVICE_INVALIDATED` and
/// friends): the stream is dead for good, but a new one can be opened.
#[derive(Debug, thiserror::Error)]
#[error("the audio endpoint behind the capture stream went away: {0}")]
pub(crate) struct SourceGone(pub String);

/// Opens capture streams, and names which one should be open right now.
pub(crate) trait SourceOpener {
    type Stream: PacketSource;

    /// Open the wanted source, returning it with its key (see [`SourceOpener::wanted`]).
    fn open(&mut self) -> Result<(Self::Stream, String)>;

    /// A key for the source that should be open now (the default endpoint's id, a target
    /// process). A key different from the open stream's means it is reopened.
    fn wanted(&mut self) -> Result<String>;
}

pub(crate) struct RecoveringSource<O: SourceOpener> {
    opener: O,
    stream: Option<(O::Stream, String)>,
    timeline: AudioTimeline,
    /// When a lost stream is next reopened, on the shared clock.
    retry_at: Duration,
    next_switch_check: Duration,
    /// Opens so far, so the owner can tell a fresh stream from the one it last looked at.
    generation: u64,
    /// Failed reopens since the stream was lost; only the first is logged as a warning.
    failed_reopens: u32,
}

impl<O: SourceOpener> RecoveringSource<O> {
    /// Open the first stream. Failing here is fatal, unlike later: a recording that never had
    /// audio should say so at start.
    pub fn start(mut opener: O, timeline: AudioTimeline, now: Duration) -> Result<Self> {
        let stream = opener.open()?;
        Ok(Self {
            opener,
            stream: Some(stream),
            timeline,
            retry_at: now,
            next_switch_check: now + SWITCH_CHECK_INTERVAL,
            generation: 1,
            failed_reopens: 0,
        })
    }

    pub fn stream(&self) -> Option<&O::Stream> {
        self.stream.as_ref().map(|(s, _)| s)
    }

    #[cfg(test)]
    pub fn opener(&self) -> &O {
        &self.opener
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn next_block(&mut self) -> Option<Block> {
        self.timeline.next_block()
    }

    /// Take what the stream has, reopen it if it is lost or no longer wanted, and pad up to `now`
    /// when nothing is flowing. Only an error that is not a lost endpoint comes back.
    pub fn poll(&mut self, now: Duration) -> Result<()> {
        if let Some((stream, key)) = self.stream.as_mut() {
            match self.timeline.drain(stream, MAX_PACKETS_PER_POLL) {
                Ok(true) => self.timeline.fill_to(now),
                Ok(false) => {}
                Err(e) if e.downcast_ref::<SourceGone>().is_some() => {
                    tracing::warn!(source = %key, error = %e, "game audio source lost; reopening, silence fills the gap");
                    self.stream = None;
                    self.retry_at = now;
                }
                Err(e) => return Err(e),
            }
        }

        if self.stream.is_some() && now >= self.next_switch_check {
            self.next_switch_check = now + SWITCH_CHECK_INTERVAL;
            self.check_switch();
        }

        if self.stream.is_none() {
            if now >= self.retry_at {
                self.reopen(now);
            }
            // Still padded when the reopen just succeeded: its first packets come on the next poll.
            self.timeline.fill_to(now);
        }
        Ok(())
    }

    fn check_switch(&mut self) {
        let Some((_, open_key)) = self.stream.as_ref() else {
            return;
        };
        match self.opener.wanted() {
            Ok(wanted) if wanted != *open_key => {
                tracing::info!(from = %open_key, to = %wanted, "game audio source changed; reopening");
                // Dropped before the new open so a device is never held twice.
                self.stream = None;
            }
            Ok(_) => {}
            // Not knowing what is wanted is no reason to drop a stream that still works.
            Err(e) => {
                tracing::debug!(error = %e, "could not tell which game audio source is wanted")
            }
        }
    }

    fn reopen(&mut self, now: Duration) {
        match self.opener.open() {
            Ok((stream, key)) => {
                tracing::info!(source = %key, failed_attempts = self.failed_reopens, "game audio source reopened");
                self.stream = Some((stream, key));
                self.generation += 1;
                self.failed_reopens = 0;
                self.next_switch_check = now + SWITCH_CHECK_INTERVAL;
            }
            Err(e) => {
                if self.failed_reopens == 0 {
                    tracing::warn!(error = %e, "game audio source could not be reopened yet; retrying, recording silence");
                } else {
                    tracing::debug!(error = %e, attempt = self.failed_reopens + 1, "game audio reopen failed");
                }
                self.failed_reopens += 1;
                self.retry_at = now + RETRY_INTERVAL;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_timeline::PacketInfo;
    use crate::AudioFormat;
    use anyhow::anyhow;
    use std::collections::VecDeque;

    const FORMAT: AudioFormat = AudioFormat {
        sample_rate: 48_000,
        channels: 2,
    };
    const BLOCK: usize = 480;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    enum Step {
        Packet(Duration, u8),
        Gone,
        Fail,
    }

    /// Replays its steps; once they run out it has nothing waiting.
    struct FakeStream(VecDeque<Step>);

    impl PacketSource for FakeStream {
        fn pull(&mut self, out: &mut Vec<u8>) -> Result<Option<PacketInfo>> {
            match self.0.pop_front() {
                None => Ok(None),
                Some(Step::Packet(at, value)) => {
                    out.extend(std::iter::repeat_n(value, BLOCK * 4));
                    Ok(Some(PacketInfo {
                        frames: BLOCK,
                        time: Some(at),
                        discontinuity: false,
                    }))
                }
                Some(Step::Gone) => Err(SourceGone("AUDCLNT_E_DEVICE_INVALIDATED".into()).into()),
                Some(Step::Fail) => Err(anyhow!("GetBuffer failed for another reason")),
            }
        }
    }

    /// Each `open` hands out the next scripted stream, or fails when the next entry is `None`.
    struct FakeOpener {
        opens: VecDeque<Option<(Vec<Step>, &'static str)>>,
        wanted: &'static str,
        attempts: u32,
    }

    impl SourceOpener for FakeOpener {
        type Stream = FakeStream;
        fn open(&mut self) -> Result<(FakeStream, String)> {
            self.attempts += 1;
            match self.opens.pop_front().flatten() {
                Some((steps, key)) => Ok((FakeStream(steps.into()), key.to_string())),
                None => Err(anyhow!("no render endpoint")),
            }
        }
        fn wanted(&mut self) -> Result<String> {
            Ok(self.wanted.to_string())
        }
    }

    fn opener(opens: Vec<Option<(Vec<Step>, &'static str)>>, wanted: &'static str) -> FakeOpener {
        FakeOpener {
            opens: opens.into(),
            wanted,
            attempts: 0,
        }
    }

    fn blocks(source: &mut RecoveringSource<FakeOpener>) -> Vec<Block> {
        std::iter::from_fn(|| source.next_block()).collect()
    }

    /// One value per frame across `blocks`.
    fn frames(blocks: &[Block]) -> Vec<u8> {
        blocks
            .iter()
            .flat_map(|b| b.buffer.data.chunks_exact(4).map(|f| f[0]))
            .collect()
    }

    #[test]
    fn an_unplugged_device_is_reopened_and_the_outage_is_silence() {
        let first = vec![Step::Packet(ms(0), 1), Step::Packet(ms(10), 1), Step::Gone];
        let second = vec![Step::Packet(ms(1_000), 2)];
        let opener = opener(
            vec![Some((first, "headset")), None, Some((second, "speakers"))],
            "speakers",
        );
        let mut source =
            RecoveringSource::start(opener, AudioTimeline::new(FORMAT, ms(0)), ms(0)).unwrap();

        source.poll(ms(20)).unwrap(); // two packets, then the device is gone
        assert!(source.stream().is_none());
        assert_eq!(
            source.opener.attempts, 2,
            "reopened at once, and that first try failed"
        );
        source.poll(ms(300)).unwrap(); // still inside the retry interval
        assert_eq!(source.opener.attempts, 2);
        source.poll(ms(600)).unwrap(); // reopened
        assert_eq!(source.generation(), 2);
        source.poll(ms(1_100)).unwrap(); // the new device's first packet, at its own time

        let blocks = blocks(&mut source);
        let frames = frames(&blocks);
        assert_eq!(
            frames.len(),
            101 * BLOCK,
            "one unbroken second plus the new packet"
        );
        assert!(frames[..2 * BLOCK].iter().all(|&s| s == 1));
        assert!(
            frames[2 * BLOCK..100 * BLOCK].iter().all(|&s| s == 0),
            "the outage is silence"
        );
        assert!(frames[100 * BLOCK..].iter().all(|&s| s == 2));
        for (i, block) in blocks.iter().enumerate() {
            assert_eq!(
                block.buffer.pts,
                ms(10 * i as u64),
                "block {i} sits on the timeline"
            );
        }
    }

    #[test]
    fn a_new_default_device_replaces_a_stream_that_still_works() {
        let opener = opener(
            vec![
                Some((vec![Step::Packet(ms(0), 1)], "speakers")),
                Some((vec![Step::Packet(ms(1_000), 2)], "headset")),
            ],
            "speakers",
        );
        let mut source =
            RecoveringSource::start(opener, AudioTimeline::new(FORMAT, ms(0)), ms(0)).unwrap();
        source.poll(ms(500)).unwrap();
        assert_eq!(source.generation(), 1);

        source.opener.wanted = "headset";
        source.poll(ms(999)).unwrap();
        assert_eq!(source.generation(), 1, "checked only once a second");
        source.poll(ms(1_000)).unwrap();
        assert_eq!(source.generation(), 2);
        assert_eq!(
            source.stream.as_ref().map(|(_, k)| k.as_str()),
            Some("headset")
        );
        source.poll(ms(1_010)).unwrap();

        let frames = frames(&blocks(&mut source));
        assert_eq!(frames.len(), 101 * BLOCK);
        assert!(frames[100 * BLOCK..].iter().all(|&s| s == 2));
    }

    #[test]
    fn retries_continue_until_a_device_appears() {
        let late = vec![Step::Packet(ms(5_000), 3)];
        let opens = std::iter::once(Some((vec![Step::Gone], "a")))
            .chain(std::iter::repeat_with(|| None).take(9))
            .chain(std::iter::once(Some((late, "b"))))
            .collect();
        let mut source =
            RecoveringSource::start(opener(opens, "b"), AudioTimeline::new(FORMAT, ms(0)), ms(0))
                .unwrap();
        for t in (0..=5_000).step_by(100) {
            source.poll(ms(t)).unwrap();
        }
        source.poll(ms(5_100)).unwrap();
        assert_eq!(
            source.opener.attempts, 11,
            "one open plus ten retries 500ms apart"
        );
        let frames = frames(&blocks(&mut source));
        assert_eq!(frames.len(), 501 * BLOCK);
        assert!(frames[500 * BLOCK..].iter().all(|&s| s == 3));
    }

    #[test]
    fn an_error_that_is_not_a_lost_device_is_returned() {
        let opener = opener(vec![Some((vec![Step::Fail], "a"))], "a");
        let mut source =
            RecoveringSource::start(opener, AudioTimeline::new(FORMAT, ms(0)), ms(0)).unwrap();
        assert!(source.poll(ms(10)).is_err());
    }

    #[test]
    fn failing_to_open_at_start_is_an_error() {
        assert!(RecoveringSource::start(
            opener(vec![None], "a"),
            AudioTimeline::new(FORMAT, ms(0)),
            ms(0)
        )
        .is_err());
    }
}
