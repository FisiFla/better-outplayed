//! The rate the pipeline runs at: one number, decided once, used by both consumers.
//!
//! # Why this type exists
//!
//! The pipeline declares its frame rate in two places at once — the encoder child's
//! `-framerate`, and the [`crate::pump::FramePacer`] that decides how often a frame may be
//! taken off the capture backend. While those two numbers were *both* the configured
//! `encode.fps`, a machine that could not encode that rate dropped the surplus in the
//! encoder's bounded queue and recorded a timeline that did not match its own duration:
//! measured on real 4K hardware, ~24fps sustained against a configured 30 with ~45% of
//! delivered frames dropped (issue #1), and a media timeline that advanced at a fraction of
//! real time, so a `pre_seconds = 10` clip covered more than ten real seconds (issue #2).
//!
//! So the rate is decided **once**, from the configured value and what the encoder was
//! actually measured to sustain, and that one value is threaded to both consumers:
//! `Recorder::start_with_measure` writes it into [`localplay_encoder::EncodeConfig::fps`] —
//! which is what builds the child's `-framerate` — and builds the pacer from the *same*
//! field, so the two cannot be computed from different inputs. See that function for where
//! the invariant lives.
//!
//! # What "adapting" is, and what it is not
//!
//! * The configured rate is never exceeded: the effective rate is `min(configured, measured)`.
//! * When the measurement is **at or above** the configured rate the pipeline behaves exactly
//!   as it always did — the configured rate wins, and nothing is quietly degraded on a
//!   machine that can keep up.
//! * `encode.adapt_fps = false` skips the measurement entirely (saving its ~1.5s of startup)
//!   and declares the configured rate regardless. That is the escape hatch, and it is also
//!   how a user *forces* a rate the probe would otherwise reduce.

use localplay_encoder::ThroughputMeasurement;

/// The rate this recording runs at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FpsDecision {
    /// What `encode.fps` asked for. Reported to the user and never used to pace or to
    /// configure the encoder — that is [`FpsDecision::effective`]'s job alone.
    configured: u32,
    /// What the throughput probe measured, when one ran. `None` with adaptation off (no
    /// probe is taken at all) and with adaptation on... only if startup did not fail: a
    /// probe that cannot produce a number is an error, never a guess.
    measurement: Option<ThroughputMeasurement>,
    /// The rate the pacer paces to and the encoder child is told. `<= configured` always.
    effective: u32,
}

impl FpsDecision {
    /// Decide the rate from what the user asked for and what the probe measured.
    ///
    /// `adapt` is `encode.adapt_fps`: with it off, the configured rate wins and any
    /// measurement (there is none in that case) is ignored. With it on, a measurement below
    /// the configured rate *reduces* the effective rate, rounded **down** — the pipeline must
    /// never declare more than the machine was measured to sustain, which is the whole point;
    /// the floor is 1, because a rate of zero is not a pipeline.
    pub fn decide(configured: u32, adapt: bool, measurement: Option<ThroughputMeasurement>) -> Self {
        // A zero configured rate drives ffmpeg's `-framerate` and `1 / fps` arithmetic; the
        // config reader refuses it, and this clamp keeps the decision total for a caller that
        // constructs one directly (a test, or a front-end with its own validation).
        let configured = configured.max(1);
        let effective = match (adapt, measurement) {
            (true, Some(m)) if m.fps < f64::from(configured) => (m.fps.floor() as u32).max(1),
            _ => configured,
        };
        Self { configured, measurement, effective }
    }

    /// The rate both consumers are given: the pacer's interval, and the encoder child's
    /// `-framerate`.
    pub fn effective(&self) -> u32 {
        self.effective
    }

    /// What `encode.fps` asked for.
    pub fn configured(&self) -> u32 {
        self.configured
    }

    /// What the probe measured, if it ran — the number the log line and the report quote.
    pub fn measurement(&self) -> Option<ThroughputMeasurement> {
        self.measurement
    }

    /// Whether adaptation reduced the rate, i.e. whether this machine could not hold what the
    /// configuration asked for. Startup says so out loud when this is true.
    pub fn reduced(&self) -> bool {
        self.effective < self.configured
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn measured(fps: f64, size: (u32, u32)) -> Option<ThroughputMeasurement> {
        Some(ThroughputMeasurement {
            fps,
            frames: (fps * 1.5) as u64,
            window: PROBE,
            source_size: size,
            output_size: size,
        })
    }

    const PROBE: Duration = Duration::from_millis(1500);

    /// The ordinary case on a machine that keeps up: the configured rate wins, unchanged, and
    /// nothing is quietly degraded. This is the "behaviour must be exactly as today" rule.
    #[test]
    fn a_machine_that_keeps_up_runs_at_exactly_the_configured_rate() {
        for measured_fps in [400.0, 60.0, 30.1] {
            let d = FpsDecision::decide(30, true, measured(measured_fps, (1920, 1080)));
            assert_eq!(
                d.effective(),
                30,
                "measured {measured_fps}fps must not raise or lower a configured 30: {d:?}"
            );
            assert!(!d.reduced());
            assert_eq!(d.configured(), 30, "the configured rate is still what is reported");
        }
    }

    /// A measurement *at* the configured rate is not a shortfall: the effective rate stays
    /// the configured one, so a machine that is exactly on the line is not shaved down.
    #[test]
    fn a_measurement_exactly_at_the_configured_rate_keeps_the_configured_rate() {
        let d = FpsDecision::decide(30, true, measured(30.0, (3840, 2160)));
        assert_eq!(d.effective(), 30);
        assert!(!d.reduced(), "there is no shortfall to report");
    }

    /// Below the configured rate, the measurement wins — rounded down, never up, because a
    /// declared rate above what was measured is the bug this whole module exists for.
    #[test]
    fn a_measurement_below_the_configured_rate_reduces_the_rate_and_rounds_down() {
        let d = FpsDecision::decide(30, true, measured(24.3, (3840, 2160)));
        assert_eq!(d.effective(), 24, "24.3 frames per second is a 24fps pipeline, not a 25");
        assert!(d.reduced());
        assert_eq!(d.configured(), 30);
        let m = d.measurement().expect("the measurement is kept for the log line");
        assert_eq!(m.source_size, (3840, 2160), "and it says what resolution it was taken at");

        // A rate below 1fps is a broken pipeline rather than a rate of zero: the pacer and
        // ffmpeg's `-framerate` both need a positive integer.
        let d = FpsDecision::decide(30, true, measured(0.4, (3840, 2160)));
        assert_eq!(d.effective(), 1);
    }

    /// Adaptation off is the escape hatch: the configured rate is declared as it always was,
    /// the ~1.5s of startup is not spent measuring, and the pipeline accepts that the encoder
    /// may drop frames on a machine that cannot hold it.
    #[test]
    fn with_adaptation_off_the_configured_rate_is_declared_whatever_was_measured() {
        let d = FpsDecision::decide(30, false, measured(12.0, (3840, 2160)));
        assert_eq!(d.effective(), 30, "no measurement may reduce the rate when adapting is off");
        assert!(!d.reduced(), "and nothing was reduced, so startup says nothing about a shortfall");
        let d = FpsDecision::decide(30, false, None);
        assert_eq!(d.effective(), 30);
        assert!(d.measurement().is_none(), "no probe ran");
    }

    /// Adaptation on with no measurement: no reduction, because there is no number to reduce
    /// to. (The shipping path never reaches this — a probe that fails stops startup — but the
    /// decision has to be total for a caller that hands it nothing.)
    #[test]
    fn no_measurement_means_no_reduction() {
        let d = FpsDecision::decide(60, true, None);
        assert_eq!(d.effective(), 60);
        assert!(!d.reduced());
    }

    /// A configured zero is clamped rather than propagated into `1 / fps` arithmetic and
    /// ffmpeg's `-framerate 0`.
    #[test]
    fn a_zero_configured_rate_is_clamped_to_one() {
        assert_eq!(FpsDecision::decide(0, true, None).effective(), 1);
        assert_eq!(FpsDecision::decide(0, false, None).configured(), 1);
    }
}
