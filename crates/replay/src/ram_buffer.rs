//! The in-memory replay ring: the last N seconds of footage, held in RAM.
//!
//! A file-based ring has ffmpeg write one MP4 a second into a scratch directory and deletes old
//! ones, so an idle buffer is a continuous trickle of SSD writes for footage usually thrown
//! away. This ring holds the same footage as a queue of fragments in memory, and nothing is
//! written to disk until a clip is saved.
//!
//! It is fed by [`crate::MemoryRingBuffer::push`] with whatever bytes the reader thread just got
//! from ffmpeg's fragmented-MP4 output (box parsing is `localplay_media::fragments`). Taking
//! bytes rather than a stream lets every rule here be tested without a pipe, child or file.
//!
//! Two rules borrowed from the file ledger:
//!
//! * A segment's end is proved by the next segment's start. A fragment's bytes do not say where
//!   it ends without parsing its sample table, while the next `tfdt` says exactly. So the
//!   newest fragment is held but its end is not claimed (the file ledger's "trusted once a
//!   strictly later one exists"), and `span_ms` lags the newest fragment by one.
//! * Eviction never empties the ring. An empty ring can never satisfy a later trigger, so
//!   slightly over budget beats unable to clip (as `SegmentLedger::evict_to_cap` decides).

use anyhow::Result;
use localplay_media::FragmentSplitter;
use std::collections::VecDeque;

/// One fragment of the ring: a `moof`+`mdat` pair, and where it sits on the timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySegment {
    /// Sequence number, counted from 0 as the ring received fragments. Not an index: eviction
    /// removes from the front, so this is how a clip's range is named regardless of what was
    /// dropped.
    pub seq: u64,
    /// Where this fragment's first sample decodes, in run-relative media time.
    pub start_ms: u64,
    /// Where its samples end: equal to `start_ms` until the next fragment arrives and proves it.
    /// See the module note.
    pub end_ms: u64,
    /// Whether the fragment starts on a keyframe. True for every fragment ingested
    /// (`frag_keyframe`), carried so a clip's first fragment can be asserted to be one.
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
    /// RAM, not disk. Named so a status line reading `bytes=` is not mistaken for a claim about the
    /// scratch directory.
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
    /// True when the requested range starts before the oldest footage the ring holds: the pre-roll
    /// asked for is longer than the buffer. Reported, as
    /// [`crate::window::SegmentWindow::truncated_front`] is.
    pub truncated_front: bool,
    /// Audio tracks the footage carries, from the header, so the save path can re-name them: a
    /// `-c copy` to a file drops per-stream metadata.
    pub audio_tracks: usize,
}

impl MemoryWindow<'_> {
    /// The checks a splice must pass, so a caller assembling the bytes itself runs the same ones.
    ///
    /// A recorder holds its ring behind a lock and a window borrows the ring, so it cannot outlive
    /// the lock. Assembling under the lock and splicing outside is the only safe order: holding it
    /// across ffmpeg would block the reader thread, filling the pipe, stopping ffmpeg draining
    /// stdin and stalling the recording. Splitting the steps lets both paths check the same
    /// invariants.
    ///
    /// Each is silent damage rather than an ffmpeg error:
    ///
    /// * a window with no fragments has nothing to cut;
    /// * a non-keyframe first fragment opens the clip mid-GOP, its first frames decoding as
    ///   garbage or not at all;
    /// * an unclosed fragment has no proven end, so the clip's duration would be a claim, not a
    ///   measurement.
    pub fn validate(&self) -> anyhow::Result<()> {
        let Some(first) = self.segments.first() else {
            anyhow::bail!("cannot splice an empty in-memory window");
        };
        if !first.keyframe {
            anyhow::bail!(
                "the first fragment of this window starts at {}ms and is not a keyframe, so a \
                 clip cut here would open mid-GOP",
                first.start_ms
            );
        }
        if let Some(open) = self.segments.iter().find(|s| !s.is_closed()) {
            anyhow::bail!(
                "the fragment starting at {}ms has no proven end, so the clip's duration would \
                 be a claim rather than a measurement",
                open.start_ms
            );
        }
        Ok(())
    }

    /// The bytes to hand ffmpeg: the header once, then each fragment in order. A valid
    /// fragmented-MP4 stream, so `ffmpeg -i this -c copy out.mp4` makes an ordinary clip without
    /// re-encoding.
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

    /// Where the window really starts, on the ring's timeline.
    ///
    /// Not the requested start: the window is cut at fragment boundaries, and the first
    /// fragment that overlaps the request begins at or *before* it. A caller that dates a clip
    /// by the request rather than by this number is off by up to one fragment.
    pub fn start_ms(&self) -> u64 {
        self.segments.first().map(|s| s.start_ms).unwrap_or(0)
    }

    /// How many audio tracks the footage carries, which `edit::audio_titles` needs because a
    /// `-c copy` to a file loses the names the encoder set.
    pub fn audio_tracks(&self) -> usize {
        self.audio_tracks
    }
}

/// The last N seconds of footage, in RAM.
pub struct MemoryRingBuffer {
    splitter: FragmentSplitter,
    segments: VecDeque<MemorySegment>,
    /// Summed `segments[i].bytes.len()`, kept rather than recomputed: the eviction loop asks after
    /// every fragment, and at 256MB that sum is not something to walk.
    bytes: u64,
    cap_bytes: u64,
    cap_ms: u64,
    evicted: u64,
    next_seq: u64,
}

impl MemoryRingBuffer {
    /// A ring holding at most `cap_bytes` of RAM and at most `cap_ms` of media; whichever bites
    /// first evicts. `cap_bytes` keeps the OS from killing the application, so size it carefully:
    /// see `BufferConfig::ram_cap_bytes`, whose default is a fraction of a typical machine's memory.
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
    /// The caller passes whatever a `read()` gave it (a partial box, three boxes, half a fragment)
    /// and the splitter holds the remainder. A fragment is admitted only once its `mdat` has fully
    /// arrived, so nothing queued has truncated samples.
    pub fn push(&mut self, chunk: &[u8]) -> Result<usize> {
        let fragments = self.splitter.push(chunk)?;
        let added = fragments.len();
        for fragment in fragments {
            // A predecessor whose sample table could not be read is closed by this fragment's start (the
            // file ledger's rule). It is now a fallback: a fragment normally knows its own length
            // (`Fragment::duration_ms`), and waiting for the next to prove it would make `span_ms` lag a
            // whole fragment behind the held footage.
            if let Some(previous) = self.segments.back_mut() {
                if !previous.is_closed() {
                    previous.end_ms = fragment.start_ms;
                }
            }
            self.bytes += fragment.bytes.len() as u64;
            self.segments.push_back(MemorySegment {
                seq: self.next_seq,
                start_ms: fragment.start_ms,
                // The fragment's own sample durations. Zero when unreadable, leaving `is_closed` false so this
                // segment is as untrusted as the file ledger's newest: the honest answer when the length is
                // unknown.
                end_ms: fragment.start_ms.saturating_add(fragment.duration_ms),
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

    /// Drop fragments off the front until both caps are satisfied, keeping at least one. Called
    /// after every push: a fragment arriving is the only moment the ring can newly exceed a cap,
    /// so the peak is one fragment over budget rather than a timer tick's worth.
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

    /// The `ftyp`+`moov` header once the splitter has seen it, `None` until then. Never evicted: a
    /// kilobyte, and the one part a clip cannot be assembled without.
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
    /// The newest fragment's end: its start plus its own measured duration. Nothing is guessed, as
    /// a fragment carries its sample durations. (When a sample table cannot be read the end falls
    /// back to the start until the next fragment supplies it; that is why
    /// [`MemorySegment::is_closed`] exists.)
    ///
    /// A trigger's range is resolved against this, as against a file ring's `stats().span_ms`.
    pub fn span_ms(&self) -> u64 {
        self.segments.back().map(|s| s.end_ms).unwrap_or(0)
    }

    /// Bytes held in RAM.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The fragments the ring holds, oldest first.
    pub fn segments(&self) -> impl Iterator<Item = &MemorySegment> {
        self.segments.iter()
    }

    /// The fragments covering `[start_ms, end_ms)`, with the header to write ahead of them.
    ///
    /// `None` when there is nothing to cut from (no header yet, or no fragment overlaps the range):
    /// a real answer, "this trigger cannot be served from this ring", not an empty clip.
    ///
    /// Only closed fragments are selected. A clip is a range of complete footage, and including the
    /// newest, whose length is unclaimed, would put an unproven end at the tail of a file whose
    /// duration then lies. A trigger wanting right up to the newest moment waits for the
    /// post-roll, as the file path does.
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
        // The pre-roll reached further back than the ring holds. Reported, not hidden: it is the
        // difference between a clip of the requested size and a shorter one because the buffer is
        // shorter than the window.
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

    /// A real fragmented-MP4 stream, generated rather than pasted: every test here is about
    /// agreeing with what ffmpeg emits (fragment boundaries, `tfdt` spacing, `moof`+`mdat`
    /// pairing), which a hand-built byte string would not test.
    fn stream(seconds: u32) -> Vec<u8> {
        let bin = localplay_media::FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
        let out = localplay_media::sidecar_command(&bin.ffmpeg)
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
            assert!(
                pair[0].is_closed(),
                "a segment with a successor has a proven end"
            );
            assert_eq!(
                pair[0].end_ms, pair[1].start_ms,
                "and that end is the successor's start — the segments are contiguous"
            );
        }
        // The newest fragment is closed too: its own sample durations carry its length, so the ring
        // knows how much footage it holds as soon as the bytes arrive, without a successor. The
        // successor-based fallback is only for a sample table this build cannot read.
        assert!(
            segments.last().unwrap().is_closed(),
            "the newest fragment claims its own measured length, so it is closed on arrival"
        );
        assert!(
            segments.iter().all(|s| s.keyframe),
            "every fragment starts a GOP"
        );

        // `span_ms` is the newest fragment's end, the media time the ring holds, not its last
        // boundary. Reading a fragment's length here instead of borrowing the next fragment's start is
        // worth a whole fragment of span, and a clip's window is resolved against it.
        assert_eq!(ring.span_ms(), segments.last().unwrap().end_ms);
        assert!(
            ring.span_ms() > 2_000,
            "four fragments of media must give a span of seconds, got {}ms",
            ring.span_ms()
        );
        assert_eq!(ring.stats().segments, segments.len());
        assert_eq!(ring.stats().ram_bytes, ring.bytes());
        assert!(
            ring.bytes() > 0,
            "the fragments' bytes are what the ring holds"
        );
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

        // The newest footage survives: eviction from the wrong end would keep the oldest second and
        // drop the moment the user triggered on.
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
        // A pipe does not deliver whole boxes. How the bytes arrive must not change what the ring
        // holds.
        let data = stream(3);
        let mut whole = MemoryRingBuffer::new(u64::MAX, u64::MAX);
        whole.push(&data).unwrap();

        let mut drip = MemoryRingBuffer::new(u64::MAX, u64::MAX);
        for byte in &data {
            drip.push(&[*byte]).unwrap();
        }

        let shape = |r: &MemoryRingBuffer| {
            r.segments()
                .map(|s| (s.seq, s.start_ms, s.end_ms, s.bytes.len()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            shape(&drip),
            shape(&whole),
            "the same ring, however the bytes arrived"
        );
        assert_eq!(drip.span_ms(), whole.span_ms());
        assert_eq!(drip.bytes(), whole.bytes());
    }

    #[test]
    fn a_window_selects_exactly_the_fragments_covering_the_range() {
        let ring = ring_of(5);
        let closed: Vec<u64> = ring
            .segments()
            .filter(|s| s.is_closed())
            .map(|s| s.start_ms)
            .collect();
        assert!(
            closed.len() >= 3,
            "need a few closed segments, got {closed:?}"
        );

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
            window
                .segments
                .iter()
                .all(|s| s.start_ms < end && s.end_ms > start),
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
        assert_eq!(
            assembled.len(),
            expected,
            "and is exactly those bytes, nothing else"
        );
        assert!(
            window.duration_ms() >= 1_000,
            "the window covers media time"
        );
    }

    #[test]
    fn a_window_starts_at_the_first_fragment_it_selected() {
        let ring = ring_of(5);
        let closed: Vec<u64> = ring
            .segments()
            .filter(|s| s.is_closed())
            .map(|s| s.start_ms)
            .collect();
        assert!(
            closed.len() >= 3,
            "need a few closed segments, got {closed:?}"
        );

        // A range that begins between two fragments: the window starts at the fragment that
        // covers it, which is the one before the request's own start.
        let start = closed[1] + 100;
        let window = ring
            .window(start, closed[2] + 1)
            .expect("a window over the middle");
        assert_eq!(
            window.start_ms(),
            closed[1],
            "the window starts where its first fragment starts, not where the request did: the \
             index row and the chapter offsets are measured from the clip's first frame"
        );
    }

    #[test]
    fn a_range_before_the_ring_began_is_reported_as_truncated() {
        // A ring that has already evicted. With a fresh 3s stream the oldest fragment starts at 0, so
        // a "long pre-roll" reaches past nothing and the truncated flag is correctly false; the case
        // only exists once the front has been dropped.
        let mut ring = MemoryRingBuffer::new(u64::MAX, 2_000);
        ring.push(&stream(5)).expect("the stream pushes");

        let oldest = ring
            .segments()
            .next()
            .expect("segments survive eviction")
            .start_ms;
        assert!(
            oldest > 0,
            "the ring must have dropped footage for this test to mean anything, and the \
             oldest surviving fragment starts at {oldest}ms"
        );

        let window = ring
            .window(0, oldest + 200)
            .expect("a window over the surviving footage");
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
        assert!(
            ring.window(0, 1_000).is_none(),
            "no header yet means no clip"
        );

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
        assert_eq!(
            ring.segments().count(),
            1,
            "and the footage itself is down to one segment"
        );
    }
}
