//! The rate the pipeline runs at: one number, decided once, used by both consumers.
//!
//! The rate is declared in two places, the encoder child's `-framerate` and the
//! [`crate::pump::FramePacer`] that decides how often a frame is taken off the capture backend.
//! It is decided once from the configured value and what the encoder was measured to sustain, and
//! `Recorder::start_with_measure` writes it into [`localplay_encoder::EncodeConfig::fps`] and builds
//! the pacer from the same field, so the two cannot come from different inputs.
//!
//! This is a pacing decision, not the timeline guarantee: paying a readback for frames the encoder
//! will drop is waste. The media timeline tracks real time independently, via
//! `-fps_mode passthrough` in `localplay_encoder::ffmpeg::video_output_args`.
//!
//! * The effective rate is `min(configured, measured)`; the configured rate is never exceeded.
//! * A measurement at or above the configured rate changes nothing.
//! * `encode.adapt_fps = false` skips the measurement (~1.5s of startup) and declares the
//!   configured rate regardless; the cost is dropped frames and the capture work spent on them.

use localplay_encoder::ThroughputMeasurement;

/// The rate this recording runs at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FpsDecision {
    /// What `encode.fps` asked for; reported only, never used to pace or configure the encoder.
    configured: u32,
    /// What the throughput probe measured. `None` with adaptation off; a probe that cannot produce a
    /// number is a startup error, never a guess.
    measurement: Option<ThroughputMeasurement>,
    /// The rate the pacer paces to and the encoder child is told. `<= configured` always.
    effective: u32,
}

impl FpsDecision {
    /// Decide the rate from what the user asked for and what the probe measured.
    ///
    /// With `adapt` off the configured rate wins. With it on, a lower measurement reduces the rate,
    /// rounded down (never declare more than was measured), with a floor of 1.
    pub fn decide(configured: u32, adapt: bool, measurement: Option<ThroughputMeasurement>) -> Self {
        // Zero would break `-framerate` and `1 / fps`; the config reader refuses it, and the clamp keeps this total for direct callers.
        let configured = configured.max(1);
        let effective = match (adapt, measurement) {
            (true, Some(m)) if m.fps < f64::from(configured) => (m.fps.floor() as u32).max(1),
            _ => configured,
        };
        Self { configured, measurement, effective }
    }

    /// The rate both consumers are given: the pacer's interval and the encoder child's `-framerate`.
    pub fn effective(&self) -> u32 {
        self.effective
    }

    /// What `encode.fps` asked for.
    pub fn configured(&self) -> u32 {
        self.configured
    }

    /// What the probe measured, if it ran.
    pub fn measurement(&self) -> Option<ThroughputMeasurement> {
        self.measurement
    }

    /// Whether adaptation reduced the rate, i.e. the machine could not hold the configured one.
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

    /// A machine that keeps up gets the configured rate, unchanged.
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

    /// A measurement at the configured rate is not a shortfall.
    #[test]
    fn a_measurement_exactly_at_the_configured_rate_keeps_the_configured_rate() {
        let d = FpsDecision::decide(30, true, measured(30.0, (3840, 2160)));
        assert_eq!(d.effective(), 30);
        assert!(!d.reduced(), "there is no shortfall to report");
    }

    /// Below the configured rate the measurement wins, rounded down, never up.
    #[test]
    fn a_measurement_below_the_configured_rate_reduces_the_rate_and_rounds_down() {
        let d = FpsDecision::decide(30, true, measured(24.3, (3840, 2160)));
        assert_eq!(d.effective(), 24, "24.3 frames per second is a 24fps pipeline, not a 25");
        assert!(d.reduced());
        assert_eq!(d.configured(), 30);
        let m = d.measurement().expect("the measurement is kept for the log line");
        assert_eq!(m.source_size, (3840, 2160), "and it says what resolution it was taken at");

        // Below 1fps is a broken pipeline, not a rate of zero.
        let d = FpsDecision::decide(30, true, measured(0.4, (3840, 2160)));
        assert_eq!(d.effective(), 1);
    }

    /// Adaptation off declares the configured rate and spends no startup time measuring.
    #[test]
    fn with_adaptation_off_the_configured_rate_is_declared_whatever_was_measured() {
        let d = FpsDecision::decide(30, false, measured(12.0, (3840, 2160)));
        assert_eq!(d.effective(), 30, "no measurement may reduce the rate when adapting is off");
        assert!(!d.reduced(), "and nothing was reduced, so startup says nothing about a shortfall");
        let d = FpsDecision::decide(30, false, None);
        assert_eq!(d.effective(), 30);
        assert!(d.measurement().is_none(), "no probe ran");
    }

    /// Adaptation on with no measurement: no reduction, since there is nothing to reduce to.
    #[test]
    fn no_measurement_means_no_reduction() {
        let d = FpsDecision::decide(60, true, None);
        assert_eq!(d.effective(), 60);
        assert!(!d.reduced());
    }

    /// A configured zero is clamped rather than propagated into `1 / fps` and `-framerate 0`.
    #[test]
    fn a_zero_configured_rate_is_clamped_to_one() {
        assert_eq!(FpsDecision::decide(0, true, None).effective(), 1);
        assert_eq!(FpsDecision::decide(0, false, None).configured(), 1);
    }
}
