//! The in-memory replay ring: the last N seconds of footage, held in RAM.
//!
//! The file-based [`crate::buffer::RingBuffer`] asks ffmpeg to write one MP4 per second into a
//! scratch directory and then deletes the old ones, so an idle replay buffer is a continuous
//! trickle of SSD writes for footage that is usually thrown away unclipped. This ring holds the
//! same footage as a queue of fragments in memory instead, and **nothing is written to disk
//! until a clip is saved**.
//!
//! It is fed by [`crate::MemoryRingBuffer::push`] with whatever bytes the reader thread just got
//! from ffmpeg's fragmented-MP4 output (see `localplay_media::fragments`, which does the box
//! parsing). The reading itself is deliberately *not* here: taking bytes rather than a stream is
//! what lets every rule below be tested without a pipe, a child process or a file.
//!
//! # The two rules it borrows from the file ledger, and why
//!
//! * **A segment's end is proved by the next segment's start.** A fragment's own bytes do not
//!   say where it ends without parsing its sample table, and the next fragment's `tfdt` says it
//!   exactly. So the newest fragment is held but its end is not claimed — the same rule the
//!   file ledger uses ("a segment is only trusted once a strictly later one exists"), and the
//!   reason `span_ms` lags the newest fragment by one.
//! * **Eviction never empties the ring.** A ring with nothing in it can never satisfy a later
//!   trigger, so being slightly over budget is better than being unable to clip at all — which
//!   is also what `SegmentLedger::evict_to_cap` decides, for the same reason.

use anyhow::Result;
use localplay_media::FragmentSplitter;
use std::collections::VecDeque;

/// One fragment of the ring: a `moof`+`mdat` pair, and where it sits on the timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySegment {
    /// Sequence number, counted from 0 as the ring received fragments.
    ///
    /// Not an index: eviction removes from the front, so this is how a clip's range is talked
    /// about without worrying about what has been dropped.
    pub seq: u64,
    /// Where this fragment's first sample decodes, in run-relative media time.
    pub start_ms: u64,
    /// Where its samples end — **equal to `start_ms` until the next fragment arrives**, because
    /// it is the next fragment's `start_ms` that proves it. See the module note.
    pub end_ms: u64,
    /// Whether the fragment starts on a keyframe. True for every fragment this ring ingests
    /// (`frag_keyframe`), carried so that a clip's first fragment can be *asserted* to be one.
    pub keyframe: bool,
    /// The fragment's bytes: its `moof` and its `mdat`, nothing else.
    pub bytes: Vec<u8>,
}

impl MemorySegment {
    /// Whether the next fragment has arrived, which is what fixes this segment's end.
    pub fn is_closed(&self) -> bool {
        self.end_ms > self.start_ms
    }
}

/// What the ring holds right now, for the status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryStats {
    pub segments: usize,
    /// **RAM**, not disk. Named for what it is rather than reusing `bytes_on_disk`, so a status
    /// line reading `bytes=` cannot be mistaken for a claim about the scratch directory.
    pub ram_bytes: u64,
    /// Media time the ring can prove it holds, run-relative. Lags the newest fragment by one,
    /// by the rule in the module note.
    pub span_ms: u64,
    /// Fragments dropped off the front to stay inside the caps.
    pub evicted: u64,
}

/// A contiguous range of the ring, ready to be written to disk as a clip.
#[derive(Debug)]
pub struct MemoryWindow<'a> {
    /// The `ftyp`+`moov` header, written once ahead of the fragments.
    pub header: &'a [u8],
    /// The fragments, oldest first, all of them closed (a clip is a range of complete footage).
    pub segments: Vec<&'a MemorySegment>,
    /// True when the requested range starts before the oldest footage the ring still holds —
    /// the pre-roll the user asked for is longer than the buffer. Reported rather than hidden,
    /// exactly as [`crate::window::SegmentWindow::truncated_front`] is.
    pub truncated_front: bool,
    /// Audio tracks the footage carries, from the header. Carried so the save path can re-name
    /// them: a `-c copy` to a file drops per-stream metadata.
    pub audio_tracks: usize,
}

impl MemoryWindow<'_> {
    /// The bytes to hand ffmpeg: the header once, then each fragment in order.
    ///
    /// This is a valid fragmented-MP4 stream, which is the whole trick — `ffmpeg -i this
    /// -c copy out.mp4` turns it into an ordinary clip without re-encoding anything.
    pub fn assemble(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            self.header.len() + self.segments.iter().map(|s| s.bytes.len()).sum::<usize>(),
        );
        out.extend_from_slice(self.header);
        for segment in &self.segments {
            out.extend_from_slice(&segment.bytes);
        }
        out
    }

    /// Media time this window covers, by the segments' own timestamps.
    pub fn duration_ms(&self) -> u64 {
        match (self.segments.first(), self.segments.last()) {
            (Some(first), Some(last)) => last.end_ms.saturating_sub(first.start_ms),
            _ => 0,
        }
    }

    /// How many audio tracks the footage carries — the count `edit::audio_titles` needs, because
    /// a `-c copy` to a file does not carry the names the encoder put on them.
    pub fn audio_tracks(&self) -> usize {
        self.audio_tracks
    }
}

/// The last N seconds of footage, in RAM.
pub struct MemoryRingBuffer {
    splitter: FragmentSplitter,
    segments: VecDeque<MemorySegment>,
    /// Summed `segments[i].bytes.len()`, kept rather than recomputed: the eviction loop asks
    /// for it after every fragment, and at 256MB that sum is not something to walk.
    bytes: u64,
    cap_bytes: u64,
    cap_ms: u64,
    evicted: u64,
    next_seq: u64,
}

impl MemoryRingBuffer {
    /// A ring holding at most `cap_bytes` of RAM and at most `cap_ms` of media.
    ///
    /// Both caps apply; whichever bites first evicts. `cap_bytes` is the one that keeps the
    /// application from being killed by the operating system, so it is the one to size
    /// carefully — see `BufferConfig::ram_cap_bytes`, whose default is deliberately a fraction
    /// of a typical machine's memory rather than all of it.
    pub fn new(cap_bytes: u64, cap_ms: u64) -> Self {
        Self {
            splitter: FragmentSplitter::new(),
            segments: VecDeque::new(),
            bytes: 0,
            cap_bytes,
            cap_ms,
            evicted: 0,
            next_seq: 0,
        }
    }

    /// Feed bytes just read from the encoder's stream; returns how many fragments completed.
    ///
    /// The caller passes whatever a `read()` gave it — a partial box, three boxes, half a
    /// fragment — and the splitter holds the remainder until it is complete. A fragment is only
    /// admitted once its `mdat` has arrived in full, so nothing in this queue is a fragment
    /// with truncated samples.
    pub fn push(&mut self, chunk: &[u8]) -> Result<usize> {
        let fragments = self.splitter.push(chunk)?;
        let added = fragments.len();
        for fragment in fragments {
            // This fragment's start is the first moment its predecessor's samples are proven
            // to have ended. See the module note on why the end is not taken from the fragment
            // being pushed.
            if let Some(previous) = self.segments.back_mut() {
                if !previous.is_closed() {
                    previous.end_ms = fragment.start_ms;
                }
            }
            self.bytes += fragment.bytes.len() as u64;
            self.segments.push_back(MemorySegment {
                seq: self.next_seq,
                start_ms: fragment.start_ms,
                // Not yet known: the next fragment proves it. Equal to `start_ms`, so
                // `is_closed` is false and no range can end here.
                end_ms: fragment.start_ms,
                keyframe: fragment.keyframe,
                bytes: fragment.bytes,
            });
            self.next_seq += 1;
        }
        if added > 0 {
            self.enforce_caps();
        }
        Ok(added)
    }

    /// Drop fragments off the front until both caps are satisfied, keeping at least one.
    ///
    /// Called after every push rather than on a timer: the moment a fragment arrives is the only
    /// moment the ring can newly exceed a cap, and doing it here means the peak is one fragment
    /// over the budget instead of one tick's worth over it.
    fn enforce_caps(&mut self) {
        while self.segments.len() > 1 {
            let (oldest_start, oldest_bytes) = {
                let oldest = self.segments.front().expect("checked non-empty");
                (oldest.start_ms, oldest.bytes.len() as u64)
            };
            let newest_start = self.segments.back().expect("checked non-empty").start_ms;
            let over_time = newest_start.saturating_sub(oldest_start) > self.cap_ms;
            let over_bytes = self.bytes > self.cap_bytes;
            if !over_time && !over_bytes {
                break;
            }
            self.segments.pop_front();
            self.bytes -= oldest_bytes;
            self.evicted += 1;
        }
    }

    /// The `ftyp`+`moov` header, once the splitter has seen it.
    ///
    /// `None` until then. Kept whole and never evicted: it is a kilobyte, and it is the one part
    /// a clip cannot be assembled without.
    pub fn header(&self) -> Option<&[u8]> {
        self.splitter.header()
    }

    pub fn stats(&self) -> MemoryStats {
        MemoryStats {
            segments: self.segments.len(),
            ram_bytes: self.bytes,
            span_ms: self.span_ms(),
            evicted: self.evicted,
        }
    }

    /// Media time the ring can prove it holds, run-relative.
    ///
    /// The newest fragment's `start_ms`: everything before it is complete and contiguous, and
    /// the newest one's own length is what the fragment after it would prove. This is the number
    /// a trigger's range is resolved against, and it is the in-memory counterpart of
    /// `RingBuffer::stats().span_ms`.
    pub fn span_ms(&self) -> u64 {
        self.segments.back().map(|s| s.start_ms).unwrap_or(0)
    }

    /// Bytes held in RAM.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The fragments the ring holds, oldest first.
    pub fn segments(&self) -> impl Iterator<Item = &MemorySegment> {
        self.segments.iter()
    }

    /// The fragments covering `[start_ms, end_ms)`, with the header to write them ahead of.
    ///
    /// `None` when there is nothing to cut from — no header yet, or no fragment overlaps the
    /// range. A caller that gets `None` has a real answer ("this trigger cannot be served from
    /// this ring") rather than an empty clip.
    ///
    /// **Only closed fragments are selected.** A clip is a range of complete footage, and the
    /// newest fragment's length is not yet claimed; including it would put a fragment with an
    /// unproven end at the tail of a file whose duration then lies. A trigger that wants right
    /// up to the newest moment waits for the post-roll, exactly as the file path does.
    pub fn window(&self, start_ms: u64, end_ms: u64) -> Option<MemoryWindow<'_>> {
        let header = self.header()?;
        let selected: Vec<&MemorySegment> = self
            .segments
            .iter()
            .filter(|s| s.is_closed())
            .filter(|s| s.start_ms < end_ms && s.end_ms > start_ms)
            .collect();
        if selected.is_empty() {
            return None;
        }
        // The pre-roll reached further back than the ring still holds. Reported, not hidden:
        // it is the difference between "a clip of the size you asked for" and "a shorter one,
        // because the buffer is shorter than the window".
        let truncated_front = selected.first().is_some_and(|s| s.start_ms > start_ms);
        Some(MemoryWindow {
            header,
            segments: selected,
            truncated_front,
            audio_tracks: self.splitter.audio_tracks(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A real fragmented-MP4 stream, generated rather than pasted.
    ///
    /// Every test below is about agreeing with what ffmpeg actually emits — fragment boundaries,
    /// `tfdt` spacing, `moof`+`mdat` pairing — so a hand-built byte string would test the ring
    /// against my idea of the format instead.
    fn stream(seconds: u32) -> Vec<u8> {
        let bin = localplay_media::FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
        let out = Command::new(&bin.ffmpeg)
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc2=size=320x180:rate=30:duration={seconds}"),
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:sample_rate=48000:duration={seconds}"),
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-g",
                "30",
                "-force_key_frames",
                "expr:gte(t,n_forced*1)",
                "-c:a",
                "aac",
                "-ac",
                "2",
                "-f",
                "mp4",
                "-movflags",
                "empty_moov+frag_keyframe+default_base_moof",
                "-",
            ])
            .output()
            .expect("spawning ffmpeg");
        assert!(out.status.success(), "ffmpeg could not generate the stream");
        out.stdout
    }

    fn ring_of(seconds: u32) -> MemoryRingBuffer {
        let mut ring = MemoryRingBuffer::new(u64::MAX, u64::MAX);
        ring.push(&stream(seconds)).expect("the stream pushes");
        ring
    }

    #[test]
    fn pushing_a_stream_fills_the_ring_with_monotonic_closed_segments() {
        let ring = ring_of(4);
        let segments: Vec<&MemorySegment> = ring.segments().collect();
        assert!(
            (3..=5).contains(&segments.len()),
            "a 4s stream at 1s fragments should be about four segments, got {}",
            segments.len()
        );

        // Every segment but the newest is closed, and closing set its end from the next one.
        for pair in segments.windows(2) {
            assert!(pair[0].is_closed(), "a segment with a successor has a proven end");
            assert_eq!(
                pair[0].end_ms, pair[1].start_ms,
                "and that end is the successor's start — the segments are contiguous"
            );
        }
        assert!(
            !segments.last().unwrap().is_closed(),
            "the newest fragment's length is not claimed until the next fragment proves it"
        );
        assert!(segments.iter().all(|s| s.keyframe), "every fragment starts a GOP");

        // `span_ms` is the newest fragment's start, which is what the ring can PROVE.
        assert_eq!(ring.span_ms(), segments.last().unwrap().start_ms);
        assert!(
            ring.span_ms() > 2_000,
            "four fragments of media must give a span of seconds, got {}ms",
            ring.span_ms()
        );
        assert_eq!(ring.stats().segments, segments.len());
        assert_eq!(ring.stats().ram_bytes, ring.bytes());
        assert!(ring.bytes() > 0, "the fragments' bytes are what the ring holds");
    }

    #[test]
    fn a_byte_cap_evicts_from_the_front_and_never_empties_the_ring() {
        // A cap of one segment plus a little: the ring must keep the newest and drop the rest.
        let all = stream(4);
        let mut one = MemoryRingBuffer::new(u64::MAX, u64::MAX);
        one.push(&all).unwrap();
        let segment_bytes = one.segments().next().unwrap().bytes.len() as u64;

        let mut ring = MemoryRingBuffer::new(segment_bytes + 1_000, u64::MAX);
        ring.push(&all).expect("the stream pushes");
        assert!(
            ring.segments().count() <= 2,
            "a cap of one segment cannot hold four, got {}",
            ring.segments().count()
        );
        assert!(
            ring.bytes() <= segment_bytes + 1_000 || ring.segments().count() == 1,
            "the ring must be inside its byte budget, or be down to its last segment"
        );
        assert!(ring.stats().evicted > 0, "eviction is reported, not silent");

        // The newest footage is what survives — an eviction from the wrong end would keep the
        // oldest second and drop the moment the user just triggered on.
        assert_eq!(
            ring.segments().last().unwrap().start_ms,
            one.segments().last().unwrap().start_ms,
            "the newest fragment must survive eviction"
        );

        // And the ring can never be emptied: one segment is kept even when it is over budget.
        let mut tiny = MemoryRingBuffer::new(1, u64::MAX);
        tiny.push(&all).expect("the stream pushes");
        assert_eq!(
            tiny.segments().count(),
            1,
            "a ring that emptied itself could never satisfy a later trigger"
        );
    }

    #[test]
    fn a_duration_cap_keeps_only_the_last_seconds() {
        // Two seconds of media, on a stream that produces about four.
        let mut ring = MemoryRingBuffer::new(u64::MAX, 2_000);
        ring.push(&stream(4)).expect("the stream pushes");

        let segments: Vec<&MemorySegment> = ring.segments().collect();
        let span = ring.span_ms() - segments.first().unwrap().start_ms;
        assert!(
            span <= 2_000 + 1_200,
            "a 2s duration cap must not hold four seconds, kept {span}ms"
        );
        assert!(
            segments.len() >= 2,
            "and must not overshoot into holding almost nothing: {} segments",
            segments.len()
        );
        assert!(
            segments.last().unwrap().start_ms >= 1_000,
            "the surviving footage must be the END of the stream, not its beginning"
        );
    }

    #[test]
    fn pushing_one_byte_at_a_time_builds_the_same_ring() {
        // A pipe does not deliver whole boxes, and a reader thread reads whatever is there.
        // How the bytes arrive must not change what the ring holds.
        let data = stream(3);
        let mut whole = MemoryRingBuffer::new(u64::MAX, u64::MAX);
        whole.push(&data).unwrap();

        let mut drip = MemoryRingBuffer::new(u64::MAX, u64::MAX);
        for byte in &data {
            drip.push(&[*byte]).unwrap();
        }

        let shape = |r: &MemoryRingBuffer| {
            r.segments().map(|s| (s.seq, s.start_ms, s.end_ms, s.bytes.len())).collect::<Vec<_>>()
        };
        assert_eq!(shape(&drip), shape(&whole), "the same ring, however the bytes arrived");
        assert_eq!(drip.span_ms(), whole.span_ms());
        assert_eq!(drip.bytes(), whole.bytes());
    }

    #[test]
    fn a_window_selects_exactly_the_fragments_covering_the_range() {
        let ring = ring_of(5);
        let closed: Vec<u64> = ring.segments().filter(|s| s.is_closed()).map(|s| s.start_ms).collect();
        assert!(closed.len() >= 3, "need a few closed segments, got {closed:?}");

        // A range covering the second and third fragments.
        let start = closed[1];
        let end = closed[2] + 1;
        let window = ring.window(start, end).expect("a window over the middle");
        assert!(
            !window.truncated_front,
            "the range starts inside the ring, so nothing was truncated"
        );
        assert!(
            window.segments.iter().all(|s| s.is_closed()),
            "a clip is a range of COMPLETE footage: no unproven ends"
        );
        assert!(
            window.segments.iter().all(|s| s.start_ms < end && s.end_ms > start),
            "every selected fragment must overlap the range"
        );

        // The assembled bytes are the header once, then the fragments in order.
        let assembled = window.assemble();
        assert!(
            assembled.starts_with(ring.header().unwrap()),
            "a clip begins with the header, or no demuxer can read it"
        );
        let mut expected = ring.header().unwrap().len();
        for segment in &window.segments {
            expected += segment.bytes.len();
        }
        assert_eq!(assembled.len(), expected, "and is exactly those bytes, nothing else");
        assert!(window.duration_ms() >= 1_000, "the window covers media time");
    }

    #[test]
    fn a_range_before_the_ring_began_is_reported_as_truncated() {
        // A ring that has already evicted. This matters: with a fresh 3s stream the oldest
        // fragment starts at 0, so a "long pre-roll" is not reaching past anything and the
        // truncated flag would be *correctly* false — the situation only exists once the front
        // has been dropped, which is exactly when a long pre-roll runs into it.
        let mut ring = MemoryRingBuffer::new(u64::MAX, 2_000);
        ring.push(&stream(5)).expect("the stream pushes");

        let oldest = ring.segments().next().expect("segments survive eviction").start_ms;
        assert!(
            oldest > 0,
            "the ring must have dropped footage for this test to mean anything, and the \
             oldest surviving fragment starts at {oldest}ms"
        );

        let window = ring.window(0, oldest + 200).expect("a window over the surviving footage");
        assert!(
            window.truncated_front,
            "a pre-roll reaching back before the oldest surviving fragment must be reported, \
             not silently shortened"
        );
        assert_eq!(
            window.segments.first().unwrap().start_ms,
            oldest,
            "and the window starts where the ring actually starts"
        );
    }

    #[test]
    fn a_range_the_ring_cannot_serve_is_none_rather_than_an_empty_clip() {
        let mut ring = MemoryRingBuffer::new(u64::MAX, u64::MAX);
        assert!(ring.window(0, 1_000).is_none(), "no header yet means no clip");

        ring.push(&stream(3)).unwrap();
        // Past the end of everything the ring holds.
        assert!(
            ring.window(60_000, 61_000).is_none(),
            "a range beyond the buffer is None — a real answer, not an empty file"
        );
    }

    #[test]
    fn the_header_is_kept_whatever_is_evicted() {
        let mut ring = MemoryRingBuffer::new(1_000, u64::MAX);
        ring.push(&stream(3)).unwrap();
        assert!(
            ring.header().is_some(),
            "the header is a kilobyte and the one part a clip cannot be assembled without, so \
             no cap may evict it"
        );
        assert_eq!(ring.segments().count(), 1, "and the footage itself is down to one segment");
    }
}
