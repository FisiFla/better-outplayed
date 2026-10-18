//! Places captured audio packets on the shared clock by their device timestamps.
//!
//! Downstream times audio by byte count alone (ffmpeg reads raw s16le), so a missing stretch of
//! samples plays everything after it early against the video. Loopback delivers no packets while
//! nothing plays and loses data when it is read late, so the stream is padded with silence (or
//! trimmed) until each packet's first sample sits where its timestamp says.

use crate::{AudioBuffer, AudioFormat};
use anyhow::Result;
use std::collections::VecDeque;
use std::time::Duration;

/// How far behind `now` the silence fill stays while no packets arrive. A packet is handed over up
/// to a device period after its first sample played, and padding over it first would trim real
/// audio; 100ms is ten periods of headroom and still nothing a clip cut notices.
pub(crate) const LIVE_LAG: Duration = Duration::from_millis(100);

/// One packet as the source described it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PacketInfo {
    pub frames: usize,
    /// When the packet's first sample played, on the shared clock. `None` when the device has no
    /// usable timestamp for it, so it simply follows the previous packet.
    pub time: Option<Duration>,
    /// The engine lost data before this packet (`AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`).
    pub discontinuity: bool,
}

/// Where packets come from: the WASAPI capture client, or a scripted fake in tests.
pub(crate) trait PacketSource {
    /// Append the next packet's samples to `out` (a silent packet as zeros) and describe it, or
    /// return `None` when nothing is waiting.
    fn pull(&mut self, out: &mut Vec<u8>) -> Result<Option<PacketInfo>>;
}

/// A 10ms block taken off the timeline.
#[derive(Debug)]
pub(crate) struct Block {
    pub buffer: AudioBuffer,
    /// Every sample is padding, not something the device delivered. Lets the converted-silence
    /// diagnostic ignore stretches where the engine sent nothing at all.
    pub filler: bool,
}

/// The game-audio timeline: frame `n` plays at `origin + n / sample_rate` on the shared clock.
pub(crate) struct AudioTimeline {
    format: AudioFormat,
    origin: Duration,
    /// Frames placed so far, emitted or still pending.
    placed: u64,
    /// Frames already handed out as blocks.
    emitted: u64,
    pending: Vec<u8>,
    /// Padded frame ranges not yet fully emitted, oldest first.
    fills: VecDeque<(u64, u64)>,
    scratch: Vec<u8>,
}

impl AudioTimeline {
    pub fn new(format: AudioFormat, origin: Duration) -> Self {
        Self {
            format,
            origin,
            placed: 0,
            emitted: 0,
            pending: Vec::new(),
            fills: VecDeque::new(),
            scratch: Vec::new(),
        }
    }

    fn bytes_per_frame(&self) -> usize {
        self.format.channels as usize * 2
    }

    fn block_frames(&self) -> u64 {
        u64::from(self.format.sample_rate / 100)
    }

    /// The frame index `time` falls on, rounded; anything before the origin is frame 0.
    fn frame_at(&self, time: Duration) -> u64 {
        let nanos = time.saturating_sub(self.origin).as_nanos();
        ((nanos * u128::from(self.format.sample_rate) + 500_000_000) / 1_000_000_000) as u64
    }

    fn time_of(&self, frame: u64) -> Duration {
        let nanos = u128::from(frame) * 1_000_000_000 / u128::from(self.format.sample_rate);
        self.origin + Duration::from_nanos(nanos as u64)
    }

    /// Pull and place up to `max` packets. Returns whether the source ran dry, the only state in
    /// which padding up to the live edge cannot overwrite audio still queued in the engine.
    pub fn drain(&mut self, source: &mut impl PacketSource, max: usize) -> Result<bool> {
        for _ in 0..max {
            let mut scratch = std::mem::take(&mut self.scratch);
            scratch.clear();
            let pulled = source.pull(&mut scratch);
            let info = match pulled {
                Ok(Some(info)) => info,
                other => {
                    self.scratch = scratch;
                    return other.map(|_| true);
                }
            };
            self.place(info, &scratch);
            self.scratch = scratch;
        }
        Ok(false)
    }

    /// Put one packet where its timestamp says. Within one block of the running count it is just
    /// appended, so timestamp jitter never chops a steady stream; a discontinuity is placed exactly
    /// because the engine has said samples are missing.
    pub fn place(&mut self, info: PacketInfo, samples: &[u8]) {
        let bytes_per_frame = self.bytes_per_frame();
        let frames = (samples.len() / bytes_per_frame).min(info.frames) as u64;
        let mut skip = 0;
        if let Some(time) = info.time {
            let at = self.frame_at(time);
            let tolerance = if info.discontinuity {
                0
            } else {
                self.block_frames()
            };
            if at > self.placed + tolerance {
                self.pad(at - self.placed);
            } else if at + tolerance < self.placed {
                // The stream already covers this stretch (padding that ran ahead of a late packet).
                skip = (self.placed - at).min(frames);
            }
        }
        let start = skip as usize * bytes_per_frame;
        let end = frames as usize * bytes_per_frame;
        self.pending.extend_from_slice(&samples[start..end]);
        self.placed += frames - skip;
    }

    /// Pad with silence up to `LIVE_LAG` before `now`, so a stretch with no packets still advances
    /// the audio at the video's pace instead of arriving later as one burst.
    pub fn fill_to(&mut self, now: Duration) {
        let at = self.frame_at(now.saturating_sub(LIVE_LAG));
        if at > self.placed {
            self.pad(at - self.placed);
        }
    }

    fn pad(&mut self, frames: u64) {
        let start = self.placed;
        let end = start + frames;
        match self.fills.back_mut() {
            Some(last) if last.1 == start => last.1 = end,
            _ => self.fills.push_back((start, end)),
        }
        let bytes = frames as usize * self.bytes_per_frame();
        self.pending.resize(self.pending.len() + bytes, 0);
        self.placed = end;
    }

    /// The next whole 10ms block, stamped from its position on the timeline.
    pub fn next_block(&mut self) -> Option<Block> {
        let block_frames = self.block_frames();
        let block_bytes = block_frames as usize * self.bytes_per_frame();
        if self.pending.len() < block_bytes {
            return None;
        }
        let data: Vec<u8> = self.pending.drain(..block_bytes).collect();
        let (start, end) = (self.emitted, self.emitted + block_frames);
        let filler = self
            .fills
            .iter()
            .any(|&(from, to)| from <= start && end <= to);
        while self.fills.front().is_some_and(|&(_, to)| to <= end) {
            self.fills.pop_front();
        }
        let pts = self.time_of(start);
        self.emitted = end;
        Some(Block {
            buffer: AudioBuffer {
                data,
                frames: block_frames as usize,
                pts,
                format: self.format,
            },
            filler,
        })
    }
}

/// A device QPC position (100ns units) on the shared clock, given one instant read on both:
/// `anchor_hns` on the QPC and `anchor` on the shared clock. `None` before the shared clock began.
pub(crate) fn qpc_to_shared(hns: u64, anchor_hns: u64, anchor: Duration) -> Option<Duration> {
    if hns >= anchor_hns {
        Some(anchor + Duration::from_nanos((hns - anchor_hns).saturating_mul(100)))
    } else {
        anchor.checked_sub(Duration::from_nanos((anchor_hns - hns).saturating_mul(100)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORMAT: AudioFormat = AudioFormat {
        sample_rate: 48_000,
        channels: 2,
    };
    const BLOCK: usize = 480;
    const ORIGIN: Duration = Duration::from_secs(5);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A packet of `frames` frames, every sample `value` (0 for a silent packet).
    fn packet(
        at: Option<Duration>,
        frames: usize,
        value: u8,
        discontinuity: bool,
    ) -> (PacketInfo, Vec<u8>) {
        (
            PacketInfo {
                frames,
                time: at,
                discontinuity,
            },
            vec![value; frames * 4],
        )
    }

    /// Plays back a scripted list of packets, then reports nothing waiting.
    struct FakeSource(VecDeque<(PacketInfo, Vec<u8>)>);

    impl FakeSource {
        fn new(packets: Vec<(PacketInfo, Vec<u8>)>) -> Self {
            Self(packets.into())
        }
    }

    impl PacketSource for FakeSource {
        fn pull(&mut self, out: &mut Vec<u8>) -> Result<Option<PacketInfo>> {
            Ok(self.0.pop_front().map(|(info, data)| {
                out.extend_from_slice(&data);
                info
            }))
        }
    }

    /// Drain `source` completely and return every block it produced.
    fn run(packets: Vec<(PacketInfo, Vec<u8>)>) -> (Vec<Block>, AudioTimeline) {
        let mut timeline = AudioTimeline::new(FORMAT, ORIGIN);
        let mut source = FakeSource::new(packets);
        while !timeline.drain(&mut source, 16).unwrap() {}
        let mut blocks = Vec::new();
        while let Some(block) = timeline.next_block() {
            blocks.push(block);
        }
        (blocks, timeline)
    }

    /// The first byte of every frame across `blocks`, in order: one value per frame on the timeline.
    fn frames(blocks: &[Block]) -> Vec<u8> {
        blocks
            .iter()
            .flat_map(|b| b.buffer.data.chunks_exact(4).map(|f| f[0]))
            .collect()
    }

    #[test]
    fn a_steady_stream_passes_through_byte_for_byte() {
        let packets = (0..10)
            .map(|i| packet(Some(ORIGIN + ms(10 * i)), BLOCK, i as u8 + 1, false))
            .collect();
        let (blocks, timeline) = run(packets);

        assert_eq!(blocks.len(), 10);
        for (i, block) in blocks.iter().enumerate() {
            assert!(
                block.buffer.data.iter().all(|&b| b == i as u8 + 1),
                "block {i} is packet {i}"
            );
            assert_eq!(block.buffer.pts, ORIGIN + ms(10 * i as u64));
            assert!(!block.filler);
        }
        assert!(timeline.pending.is_empty());
    }

    #[test]
    fn jitter_within_a_block_does_not_move_the_stream() {
        // Timestamps 3ms early and late: still contiguous, nothing padded or trimmed.
        let packets = vec![
            packet(Some(ORIGIN), BLOCK, 1, false),
            packet(Some(ORIGIN + ms(13)), BLOCK, 2, false),
            packet(Some(ORIGIN + ms(17)), BLOCK, 3, false),
        ];
        let (blocks, _) = run(packets);
        assert_eq!(
            frames(&blocks),
            [vec![1; BLOCK], vec![2; BLOCK], vec![3; BLOCK]].concat()
        );
    }

    #[test]
    fn a_gap_between_packets_is_filled_with_silence() {
        // Nothing played for 40ms: the second packet must start at frame 2400, not 480.
        let packets = vec![
            packet(Some(ORIGIN), BLOCK, 1, false),
            packet(Some(ORIGIN + ms(50)), BLOCK, 2, false),
        ];
        let (blocks, _) = run(packets);
        let frames = frames(&blocks);

        assert_eq!(frames.len(), 6 * BLOCK);
        assert!(frames[..BLOCK].iter().all(|&s| s == 1));
        assert!(
            frames[BLOCK..5 * BLOCK].iter().all(|&s| s == 0),
            "40ms of silence"
        );
        assert!(frames[5 * BLOCK..].iter().all(|&s| s == 2));
        let filler: Vec<bool> = blocks.iter().map(|b| b.filler).collect();
        assert_eq!(filler, [false, true, true, true, true, false]);
        assert_eq!(
            blocks[5].buffer.pts,
            ORIGIN + ms(50),
            "the late packet keeps its own time"
        );
    }

    #[test]
    fn a_discontinuity_is_placed_exactly_even_under_the_jitter_tolerance() {
        // 5ms (240 frames) missing: ignored as jitter without the flag, filled exactly with it.
        let gap = |discontinuity| {
            let packets = vec![
                packet(Some(ORIGIN), BLOCK, 1, false),
                packet(Some(ORIGIN + ms(15)), BLOCK, 2, discontinuity),
            ];
            let mut timeline = AudioTimeline::new(FORMAT, ORIGIN);
            while !timeline
                .drain(&mut FakeSource::new(packets.clone()), 16)
                .unwrap()
            {}
            timeline.placed
        };
        assert_eq!(gap(false), 2 * BLOCK as u64);
        assert_eq!(gap(true), 2 * BLOCK as u64 + 240);
    }

    #[test]
    fn a_silent_packet_holds_its_place_on_the_timeline() {
        // The source writes a silent packet as zeros; it is real time, not a gap, so nothing is padded.
        let packets = vec![
            packet(Some(ORIGIN), BLOCK, 1, false),
            packet(Some(ORIGIN + ms(10)), 2 * BLOCK, 0, false),
            packet(Some(ORIGIN + ms(30)), BLOCK, 3, false),
        ];
        let (blocks, _) = run(packets);
        assert_eq!(
            frames(&blocks),
            [vec![1; BLOCK], vec![0; 2 * BLOCK], vec![3; BLOCK]].concat()
        );
        assert!(
            blocks.iter().all(|b| !b.filler),
            "device silence is not filler"
        );
    }

    #[test]
    fn a_packet_overlapping_what_is_already_placed_is_trimmed() {
        // The second packet claims to start 20ms after the first, which already covers 40ms.
        let packets = vec![
            packet(Some(ORIGIN), 4 * BLOCK, 1, false),
            packet(Some(ORIGIN + ms(20)), 3 * BLOCK, 2, false),
        ];
        let (blocks, _) = run(packets);
        let frames = frames(&blocks);
        assert_eq!(frames.len(), 5 * BLOCK, "only the part past 40ms survives");
        assert!(frames[4 * BLOCK..].iter().all(|&s| s == 2));
    }

    #[test]
    fn a_packet_without_a_timestamp_follows_the_previous_one() {
        let packets = vec![
            packet(Some(ORIGIN), BLOCK, 1, false),
            packet(None, BLOCK, 2, false),
        ];
        let (blocks, _) = run(packets);
        assert_eq!(frames(&blocks), [vec![1; BLOCK], vec![2; BLOCK]].concat());
    }

    #[test]
    fn with_no_packets_silence_keeps_pace_with_the_clock_behind_the_live_edge() {
        let mut timeline = AudioTimeline::new(FORMAT, ORIGIN);
        assert!(timeline.drain(&mut FakeSource::new(vec![]), 16).unwrap());
        timeline.fill_to(ORIGIN + Duration::from_secs(1));

        let mut blocks = Vec::new();
        while let Some(block) = timeline.next_block() {
            blocks.push(block);
        }
        assert_eq!(blocks.len(), 90, "one second less the 100ms lag");
        assert!(blocks
            .iter()
            .all(|b| b.filler && b.buffer.data.iter().all(|&s| s == 0)));

        // The engine resumes: its first packet lands at its own time, after the rest of the gap.
        let resumed = packet(Some(ORIGIN + Duration::from_secs(1)), BLOCK, 7, false);
        timeline
            .drain(&mut FakeSource::new(vec![resumed]), 16)
            .unwrap();
        assert_eq!(timeline.placed, 48_000 + BLOCK as u64);
        let mut tail = Vec::new();
        while let Some(block) = timeline.next_block() {
            tail.push(block);
        }
        assert_eq!(tail.len(), 11);
        assert!(tail[..10].iter().all(|b| b.filler));
        assert_eq!(tail[10].buffer.pts, ORIGIN + Duration::from_secs(1));
        assert!(!tail[10].filler && tail[10].buffer.data.iter().all(|&s| s == 7));
    }

    #[test]
    fn filling_never_runs_ahead_of_the_live_lag() {
        let mut timeline = AudioTimeline::new(FORMAT, ORIGIN);
        timeline.fill_to(ORIGIN + LIVE_LAG);
        assert_eq!(timeline.placed, 0);
        timeline.fill_to(ORIGIN); // before the lag has passed: nothing, not an underflow
        assert_eq!(timeline.placed, 0);
    }

    #[test]
    fn drain_reports_a_source_that_still_has_packets() {
        let packets = (0..20)
            .map(|i| packet(Some(ORIGIN + ms(10 * i)), BLOCK, 1, false))
            .collect();
        let mut source = FakeSource::new(packets);
        let mut timeline = AudioTimeline::new(FORMAT, ORIGIN);
        assert!(
            !timeline.drain(&mut source, 16).unwrap(),
            "four packets are still waiting"
        );
        assert!(timeline.drain(&mut source, 16).unwrap());
        assert_eq!(timeline.placed, 20 * BLOCK as u64);
    }

    #[test]
    fn a_qpc_position_converts_onto_the_shared_clock() {
        let anchor = Duration::from_secs(10);
        assert_eq!(qpc_to_shared(1_000_000, 1_000_000, anchor), Some(anchor));
        assert_eq!(
            qpc_to_shared(1_000_010, 1_000_000, anchor),
            Some(anchor + Duration::from_micros(1))
        );
        assert_eq!(
            qpc_to_shared(900_000, 1_000_000, anchor),
            Some(anchor - ms(10))
        );
        assert_eq!(
            qpc_to_shared(0, 200_000_000, anchor),
            None,
            "before the shared clock began"
        );
    }
}
