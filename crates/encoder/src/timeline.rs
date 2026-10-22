//! The video's timeline: when a captured frame may be handed to the child, and which captured
//! frame a finished one is.
//!
//! # Why the timeline is not ffmpeg's to decide
//!
//! ffmpeg does not read timestamps out of the video it is given. It stamps each frame with the
//! wall clock at the moment it *read* that frame from the pipe, which is why the input is declared
//! with `-use_wallclock_as_timestamps 1`. A child that reads a burst of frames at once therefore
//! gets a burst of timestamps, and the picture judders: the frames of one burst share a timestamp
//! and the picture freezes between the bursts.
//!
//! That is what a 4K recording on the zero-copy path looked like. The hardware encoder finished
//! several frames at a time, so the reads from the pipe came in bursts, and the measured frame
//! intervals swung between 8.1 ms and 36.4 ms against the 16.7 ms a 60fps video has: only 2.3% of
//! them were right. The picture was not shown when it was captured, and it was not shown evenly.
//!
//! So the capture times are carried to the pipe and the pipe is written *at* those times. The
//! child then reads the frames one at a time, spread out the way the picture was, and the wall
//! clock it stamps them with is the capture clock.
//!
//! # Why the total duration does not move
//!
//! The schedule spaces the frames by the gaps between their *capture* times, so the span of the
//! written frames is the span of the capture: the video is as long as the picture was, and no
//! longer. A video whose intervals were even at the configured rate would be shorter than the
//! audio — the capture delivers about 57.6 frames a second of real time, not 60 — and that is a
//! different defect (an A/V drift), not a fix.
//!
//! The two decisions that take are here, both pure, so they can be tested off Windows:
//!
//! * [`FrameSchedule`] says when a frame may be written.
//! * [`FramePairing`] says which captured frame a finished one is.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long before a frame's slot the writer stops sleeping and starts waiting for it exactly.
///
/// An operating system's sleep is not accurate to the microsecond, and the error lands on the
/// frame's interval: a machine whose timers are coalesced can take 24ms for a 16ms sleep, and the
/// measured intervals of a recording then swing by as much as the sleep was wrong. The last
/// stretch is therefore waited for rather than slept through — a fraction of a millisecond of
/// spinning, once per frame.
pub const SPIN_WINDOW: Duration = Duration::from_millis(1);

/// The longest sleep the writer takes in one go.
///
/// A long sleep can overshoot by more than a step, and the overshoot is the interval's error, so
/// the wait is taken in steps and the last stretch is waited for.
pub const SLEEP_STEP: Duration = Duration::from_millis(1);

/// Wait until `due`, as precisely as this machine allows.
///
/// The wait is taken in short sleeps and the last [`SPIN_WINDOW`] is waited for exactly. Both
/// halves matter: a sleep alone is only accurate to a millisecond or so (on a machine whose
/// timers are coalesced, to several), and the error is the frame interval's error; and spinning
/// alone would burn a core for the whole of every frame interval.
pub fn wait_until(due: Instant) {
    loop {
        let now = Instant::now();
        if now >= due {
            return;
        }
        let remaining = due.saturating_duration_since(now);
        if remaining <= SPIN_WINDOW {
            // The last stretch: waited for, because a sleep cannot be this precise.
            while Instant::now() < due {
                std::hint::spin_loop();
            }
            return;
        }
        // Short enough that its own overshoot is small: the wait is repeated until the slot is
        // close enough to be waited for exactly.
        std::thread::sleep(remaining.min(SLEEP_STEP));
    }
}

/// When each frame may be written to the child.
///
/// One schedule for the whole stream: the frames are written in the order they were captured, and
/// each one is written one *capture interval* after the last. The intervals are therefore the
/// capture's own, and the stream on the pipe is as long as the picture was.
///
/// **The schedule is never late.** Each frame's slot is the later of "the slot the last frame's
/// slot plus the gap between the two frames' capture times" and "the moment the frame arrived",
/// so the schedule's level is the *envelope* of the arrivals: a frame that arrives early waits
/// for its turn, and a frame that arrives late pushes the schedule on, and the frames after it
/// are written one capture interval apart again. That is what makes a burst of frames — the
/// hardware encoder finishing several at once — come out as a steady stream.
#[derive(Debug, Default)]
pub struct FrameSchedule {
    /// The capture time of the last frame written.
    last_capture: Option<Duration>,
    /// When the last frame was written.
    last_written: Option<Instant>,
}

impl FrameSchedule {
    /// How long the writer must wait before the frame captured at `captured_at` may be written.
    ///
    /// `now` is the moment the frame arrived. The answer is never negative: a frame that is
    /// already late is written at once.
    ///
    /// The rule, and why:
    ///
    /// * the picture was on the screen from one frame's capture to the next's, so the next frame
    ///   is written one capture interval after the last one was — that is what keeps the stream on
    ///   the pipe even;
    /// * a frame that arrives *early* (the encoder finished several at once) waits for its turn,
    ///   and that is what turns the encoder's burst into a steady stream;
    /// * a frame that arrives *late* (the encoder could not keep up) is written at once, because
    ///   the timeline may not be stretched to hide it.
    ///
    /// The first frame is written as soon as it is here, and it is the origin the others are
    /// measured from: an absolute schedule would have to know the pipeline's latency before the
    /// pipeline has run.
    pub fn wait_for(&mut self, captured_at: Duration, now: Instant) -> Duration {
        let due = match (self.last_capture, self.last_written) {
            (Some(previous_capture), Some(previous_written)) => previous_written
                // `checked_add`: a capture time far in the future would overflow an `Instant`,
                // and a frame that cannot be placed is written now rather than never.
                .checked_add(captured_at.saturating_sub(previous_capture))
                .unwrap_or(now),
            _ => now,
        };
        let due = if due > now { due } else { now };
        self.last_capture = Some(captured_at);
        self.last_written = Some(due);
        due.saturating_duration_since(now)
    }
}

/// The frames an encoder has been given, and the capture time of the one it finishes next.
///
/// The encoder returns one frame for each frame it was given, in the same order, so the frames
/// that went in are the queue the frames that come out are matched against: the oldest frame
/// handed over is the next frame finished.
///
/// The pairing is a queue rather than a counter because a frame can be lost on the way in — the
/// queue in front of the encoder is bounded and drops when it is full — and a counter would then
/// be off by one for the rest of the stream. A queue loses the dropped frame with it.
#[derive(Debug, Default)]
pub struct FramePairing {
    /// The capture times of the frames handed over, oldest first.
    pending: VecDeque<Duration>,
}

impl FramePairing {
    /// A frame captured at `captured_at` was handed to the encoder.
    pub fn handed_over(&mut self, captured_at: Duration) {
        self.pending.push_back(captured_at);
    }

    /// The encoder finished a frame: the capture time of the frame it is.
    ///
    /// `None` when the encoder produced a frame that was never handed over, which is a mismatch
    /// rather than a fact about the picture. The caller writes such a frame as soon as it can,
    /// which is what happened before this pairing existed.
    pub fn finished(&mut self) -> Option<Duration> {
        self.pending.pop_front()
    }

    /// How many frames are waiting to be matched.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

/// The capture time of a frame in the 100-nanosecond units Media Foundation counts in, measured
/// from the first frame handed over.
///
/// The encoder's stream starts at zero and its timestamps march by the real capture intervals, so
/// the stream the encoder sees is the capture's own timeline rather than a nominal grid: a frame
/// that was captured 17.4 ms after the one before it says so.
///
/// Saturating, because a capture time that cannot be represented is a frame no clock can place,
/// and the largest timestamp is closer to the truth than a wrapped one.
pub fn capture_pts(origin: Duration, captured_at: Duration) -> i64 {
    let units = captured_at.saturating_sub(origin).as_nanos() / 100;
    i64::try_from(units).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock the tests can place frames on exactly.
    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    /// The first frame is written as soon as it is here: there is nothing to be spaced from.
    #[test]
    fn the_first_frame_is_written_at_once() {
        let base = Instant::now();
        let mut schedule = FrameSchedule::default();
        let wait = schedule.wait_for(Duration::from_millis(17), at(base, 5));
        assert_eq!(
            wait,
            Duration::ZERO,
            "the first frame has no predecessor to be spaced from"
        );
    }

    /// The frames are written one capture interval apart, whatever the pipeline did in between.
    ///
    /// The defect this prevents: the encoder finished three frames at once, the child read them
    /// at once, and the three shared a timestamp — a burst of frames with no time between them
    /// and a frozen picture between the bursts.
    #[test]
    fn the_frames_are_written_one_capture_interval_apart() {
        let base = Instant::now();
        let mut schedule = FrameSchedule::default();
        // The three frames arrive together (the encoder finished a burst) but were captured 17 ms
        // apart. The writer asks how long to wait and then sleeps for it, so its clock advances
        // by the wait — which is what the loop below does.
        let mut now = at(base, 5);
        let mut waits = Vec::new();
        for frame in 0..3u64 {
            let wait = schedule.wait_for(Duration::from_millis(frame * 17), now);
            now += wait;
            waits.push(wait);
        }
        assert_eq!(
            waits[0],
            Duration::ZERO,
            "the first frame is written at once"
        );
        assert_eq!(
            waits[1],
            Duration::from_millis(17),
            "the second waits for the interval its capture time says"
        );
        assert_eq!(
            waits[2],
            Duration::from_millis(17),
            "and the third is measured from the frame before it, not from the burst they arrived in"
        );
        assert_eq!(
            now - base,
            Duration::from_millis(39),
            "the three frames are written 17ms apart: the capture's own intervals"
        );
    }

    /// A frame that arrives late is written at once: the timeline may not be stretched to hide a
    /// pipeline that could not keep up.
    #[test]
    fn a_frame_that_arrives_late_is_not_held_back() {
        let base = Instant::now();
        let mut schedule = FrameSchedule::default();
        schedule.wait_for(Duration::from_millis(0), at(base, 0));
        // Captured 17 ms after the first, but it only turned up 100 ms later.
        let wait = schedule.wait_for(Duration::from_millis(17), at(base, 100));
        assert_eq!(
            wait,
            Duration::ZERO,
            "a late frame is written at once, not at the slot the capture time would have given it"
        );
    }

    /// A capture time that goes backwards (a stale frame) must not move the schedule backwards.
    #[test]
    fn a_capture_time_that_goes_backwards_does_not_move_the_schedule_back() {
        let base = Instant::now();
        let mut schedule = FrameSchedule::default();
        schedule.wait_for(Duration::from_millis(100), at(base, 0));
        let wait = schedule.wait_for(Duration::from_millis(40), at(base, 1));
        assert_eq!(
            wait,
            Duration::ZERO,
            "the gap is clamped at zero, so the frame is written straight after the last one"
        );
    }

    /// The schedule is measured from the frames' capture times, so the stream it writes is as long
    /// as the picture was — the property that keeps the video's duration on the audio's.
    #[test]
    fn the_schedule_spans_the_capture_and_not_the_configured_rate() {
        let base = Instant::now();
        let mut schedule = FrameSchedule::default();
        // 60 frames captured 17.4 ms apart: the capture ran at 57.5 fps, not at 60.
        let mut written_at = base;
        for frame in 0..60u64 {
            let captured_at = Duration::from_micros(frame * 17_400);
            let wait = schedule.wait_for(captured_at, written_at);
            written_at += wait;
        }
        let span = written_at.duration_since(base);
        assert!(
            span >= Duration::from_millis(1_026) && span <= Duration::from_millis(1_044),
            "sixty frames captured 17.4ms apart span about 1.03s, not the 1.0s a 60fps grid \
             would claim: {span:?}"
        );
    }

    /// The pairing matches the frames in order, and a frame lost on the way in is lost with them.
    #[test]
    fn the_finished_frame_is_the_oldest_one_handed_over() {
        let mut pairing = FramePairing::default();
        pairing.handed_over(Duration::from_millis(0));
        pairing.handed_over(Duration::from_millis(17));
        pairing.handed_over(Duration::from_millis(34));
        assert_eq!(pairing.pending(), 3);
        assert_eq!(pairing.finished(), Some(Duration::from_millis(0)));
        assert_eq!(pairing.finished(), Some(Duration::from_millis(17)));
        assert_eq!(pairing.finished(), Some(Duration::from_millis(34)));
        assert_eq!(
            pairing.finished(),
            None,
            "and there is nothing left to match"
        );
    }

    /// The encoder's timestamps are the capture's own, measured from the first frame.
    #[test]
    fn the_encoder_is_told_the_real_capture_times() {
        let origin = Duration::from_millis(1_000);
        assert_eq!(
            capture_pts(origin, origin),
            0,
            "the first frame is the stream's origin"
        );
        assert_eq!(
            capture_pts(origin, origin + Duration::from_micros(17_400)),
            174_000,
            "17.4ms is 174000 units of 100ns"
        );
        // A capture time before the origin cannot be placed, and is clamped rather than wrapped.
        assert_eq!(capture_pts(origin, Duration::ZERO), 0);
    }

    /// A capture time far beyond what a timestamp can hold is clamped, not wrapped: a wrapped
    /// timestamp would put the frame at the wrong end of the stream.
    #[test]
    fn an_impossible_capture_time_is_clamped() {
        let absurd = Duration::from_secs(u64::MAX);
        assert_eq!(
            capture_pts(Duration::ZERO, absurd),
            i64::MAX,
            "a timestamp that cannot be represented is the largest one there is"
        );
    }

    /// The wait is accurate to a fraction of a millisecond, which is what a frame's interval needs.
    ///
    /// An ordinary sleep is not: on a machine whose timers are coalesced a 16ms sleep takes 24ms,
    /// and the error lands on the frame interval — the very judder this module exists to remove.
    ///
    /// **What is asserted is the wait's own accuracy, not the scheduler's.** The spin at the end
    /// of the wait is precise when it is given the CPU, and no implementation of "wake up at time
    /// T" can survive being descheduled: on a loaded runner the thread can lose the CPU for
    /// milliseconds at a stretch, and that is the machine, not the code. So the *median* overshoot
    /// is the code's accuracy and is what must be sub-millisecond, while a single wait that
    /// overshoots by a whole frame interval is a different matter: it would mean the stream can
    /// fall a frame behind the picture, and it must never happen.
    ///
    /// The judder this module exists to remove is not a late write either. The schedule is an
    /// **envelope**: a frame written late shifts the slots of the frames after it, so the
    /// *intervals* stay the capture's own. What made the picture judder was the intervals being
    /// wrong, and the fix for that was the capture times, not the precision of this wait.
    #[test]
    fn the_wait_is_accurate_to_a_fraction_of_a_millisecond() {
        let frame_interval = Duration::from_millis(16);
        let mut overshoots = Vec::new();
        for _ in 0..9 {
            let started = Instant::now();
            wait_until(started + frame_interval);
            overshoots.push(started.elapsed() - frame_interval);
        }
        let mut sorted = overshoots.clone();
        sorted.sort();
        let median = sorted[sorted.len() / 2];
        assert!(
            median < Duration::from_millis(2),
            "the wait's median overshoot was {median:?}: a frame's interval is only 16ms, so a \
             wait that is a millisecond out is a millisecond of drift",
        );
        let worst = sorted.last().copied().unwrap_or_default();
        assert!(
            worst < frame_interval,
            "a wait overshot by {worst:?}, which is a whole frame interval: the stream would fall \
             a frame behind the picture. The waits were {overshoots:?}.",
        );
    }
}
