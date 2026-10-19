//! Resolving a trigger instant into the set of segments to concatenate.

use crate::ledger::{Segment, SegmentLedger};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WindowError {
    #[error("the replay buffer is empty; nothing has been captured yet")]
    EmptyBuffer,
    #[error(
        "post-roll not yet on disk: needed capture timeline to reach {needed_ms}ms, \
         but only {available_ms}ms is available"
    )]
    PostRollUnavailable { needed_ms: u64, available_ms: u64 },
}

/// The segments to concatenate. Boundaries are segment-aligned by construction —
/// stream copy can only cut on keyframes, and segment starts are keyframes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentWindow {
    pub segments: Vec<Segment>,
    /// True when the requested pre-roll extends before the start of the buffer.
    pub truncated_front: bool,
}

impl SegmentWindow {
    pub fn seqs(&self) -> Vec<u64> {
        self.segments.iter().map(|s| s.seq).collect()
    }

    /// Nominal duration, accurate to within one segment.
    pub fn duration_ms(&self) -> u64 {
        self.segments.iter().map(|s| s.duration_ms).sum()
    }

    /// Where the window really starts, on the ledger's timeline.
    ///
    /// Not the requested start: a window is cut at segment boundaries, and the first segment
    /// that overlaps the request begins at or *before* it. A caller that dates a clip by the
    /// request rather than by this number is off by up to one segment, which is exactly the
    /// error the index row and the chapter offsets must not carry.
    pub fn start_ms(&self) -> u64 {
        self.segments.first().map(|s| s.start_ms).unwrap_or(0)
    }
}

/// Resolve `[trigger_ms - pre_ms, trigger_ms + post_ms]` against the ledger.
pub fn resolve(
    ledger: &SegmentLedger,
    trigger_ms: u64,
    pre_ms: u64,
    post_ms: u64,
) -> Result<SegmentWindow, WindowError> {
    if ledger.is_empty() {
        return Err(WindowError::EmptyBuffer);
    }

    let available_ms = ledger.span_ms();
    // Saturating: the request comes from a configuration and a trigger, and a window that
    // overflows the timeline must fail as "no post-roll", not wrap into the past.
    let needed_ms = trigger_ms.saturating_add(post_ms);
    if available_ms < needed_ms {
        return Err(WindowError::PostRollUnavailable {
            needed_ms,
            available_ms,
        });
    }

    let want_start = trigger_ms.saturating_sub(pre_ms);
    let want_end = needed_ms;
    // `saturating_sub` clamps a pre-roll that reaches before t=0 down to 0, so
    // compare the *unclamped* request against the buffer's first segment to still
    // detect (and report) a short pre-roll.
    let buffer_start = ledger.segments()[0].start_ms;
    let truncated_front = trigger_ms < pre_ms.saturating_add(buffer_start);

    let segments: Vec<Segment> = ledger
        .segments()
        .iter()
        .filter(|s| s.end_ms() > want_start && s.start_ms < want_end)
        .cloned()
        .collect();

    Ok(SegmentWindow {
        segments,
        truncated_front,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Segment;

    fn ledger(n: u64) -> SegmentLedger {
        let mut l = SegmentLedger::default();
        for seq in 0..n {
            l.push(Segment {
                seq,
                file: format!("seg-{seq:06}.mp4").into(),
                start_ms: seq * 1000,
                duration_ms: 1000,
                bytes: 10,
            });
        }
        l
    }

    #[test]
    fn selects_whole_segments_covering_the_window() {
        let w = resolve(&ledger(20), 10_000, 5_000, 5_000).unwrap();
        assert_eq!(w.seqs(), vec![5, 6, 7, 8, 9, 10, 11, 12, 13, 14]);
        assert!(!w.truncated_front);
    }

    #[test]
    fn the_window_starts_at_the_first_segment_it_selected() {
        // The request begins half a second into a segment, so the segment that covers it
        // starts before the request does.
        let w = resolve(&ledger(20), 10_500, 5_000, 5_000).unwrap();
        assert_eq!(w.seqs(), vec![5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        assert_eq!(
            w.start_ms(),
            5_000,
            "the window starts where its first segment starts, not where the request did: the \
             index row and the chapter offsets are measured from the clip's first frame"
        );
    }

    #[test]
    fn clamps_and_reports_when_the_window_starts_before_the_buffer() {
        // 3s of buffer, but 30s of pre-roll requested.
        let w = resolve(&ledger(3), 2_000, 30_000, 1_000).unwrap();
        assert_eq!(w.seqs(), vec![0, 1, 2]);
        assert!(
            w.truncated_front,
            "caller must be able to warn about a short pre-roll"
        );
    }

    #[test]
    fn fails_when_the_post_roll_has_not_been_written_yet() {
        // Trigger at 2s with 5s post-roll needs the timeline to reach 7s; only 3s exists.
        let err = resolve(&ledger(3), 2_000, 30_000, 5_000).unwrap_err();
        assert!(matches!(err, WindowError::PostRollUnavailable { .. }));
    }

    #[test]
    fn fails_on_an_empty_buffer() {
        let err = resolve(&SegmentLedger::default(), 1_000, 30_000, 5_000).unwrap_err();
        assert!(matches!(err, WindowError::EmptyBuffer));
    }
}
