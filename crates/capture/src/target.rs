//! What to capture: which monitor, or which window.
//!
//! Kept free of `#[cfg(windows)]` so the choosing rules are tested on every host; only the
//! enumeration that feeds them (`wgc`) is Windows. Off Windows the stub is the only video
//! backend and has no screen to choose from, so a non-default target is reported, not obeyed.

use anyhow::{bail, Result};

/// The capture target `[capture]` selects. The default is the primary monitor, which is what
/// this crate captured before the target existed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CaptureTarget {
    /// Index into [`ordered_monitors`]: 0 is the primary monitor.
    pub monitor: usize,
    /// A window title to capture instead of a whole monitor. Matched case-insensitively
    /// ([`pick_window`]); `None` captures the monitor.
    pub window: Option<String>,
}

impl CaptureTarget {
    /// Whether this is the primary monitor, i.e. what a configuration without `[capture]` gets.
    pub fn is_default(&self) -> bool {
        self.monitor == 0 && self.window.is_none()
    }

    /// Build from the config strings: an empty or whitespace-only window means "no window".
    pub fn from_config(monitor: usize, window: &str) -> Self {
        let window = window.trim();
        Self {
            monitor,
            window: (!window.is_empty()).then(|| window.to_string()),
        }
    }
}

/// Monitors in the order `[capture] monitor` indexes them: the primary first, the rest in
/// the order the OS enumerated them. The OS's own order does not promise the primary comes
/// first, and index 0 has to mean "primary" for a configuration to keep its old meaning.
pub fn ordered_monitors<T: Copy + PartialEq>(primary: T, enumerated: &[T]) -> Vec<T> {
    let mut out = vec![primary];
    out.extend(enumerated.iter().copied().filter(|m| *m != primary));
    out
}

/// One display the capture can target.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DisplayInfo {
    /// The index `[capture] monitor` uses for this display (0 is the primary).
    pub index: usize,
    /// The display's current mode width, in pixels. 0 when the OS would not describe the mode.
    pub width: u32,
    /// The display's current mode height, in pixels. 0 when the OS would not describe the mode.
    pub height: u32,
    /// The mode's refresh rate in Hz, or `None` when the OS does not report a real one.
    pub refresh_hz: Option<u32>,
}

/// A display's current mode, as the operating system reports it.
///
/// The rate is raw because the OS does not always report a rate at all: see
/// [`reported_refresh_rate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayMode {
    /// The mode's width in pixels.
    pub width: u32,
    /// The mode's height in pixels.
    pub height: u32,
    /// The rate the OS reports, in Hz.
    pub frequency: u32,
}

/// The highest refresh rate worth believing, in Hz.
///
/// The fastest displays that ship run at a few hundred Hz, so a driver reporting thousands of
/// them is reporting a number that means nothing. An unknown rate is honest; a wrong one is not.
pub const MAX_PLAUSIBLE_REFRESH_HZ: u32 = 1000;

/// The refresh rate a display mode reports, or `None` when it does not report a real one.
///
/// Windows answers 0 or 1 for a mode that inherits the display's default rate rather than naming
/// one, so neither is a rate a person could pick from: both are unknown, not "0 Hz" and "1 Hz".
///
/// A rate above [`MAX_PLAUSIBLE_REFRESH_HZ`] is unknown for the same reason: the number is not
/// one any display runs at.
pub fn reported_refresh_rate(frequency: u32) -> Option<u32> {
    if frequency < 2 {
        return None;
    }
    if frequency > MAX_PLAUSIBLE_REFRESH_HZ {
        return None;
    }
    Some(frequency)
}

/// The displays to report, numbered in the order `[capture] monitor` indexes them.
///
/// `modes` are the current modes in that same order, so a mode's position is its
/// [`DisplayInfo::index`]. A rate the OS did not report stays unknown here rather than being
/// guessed at.
pub fn indexed_displays(modes: &[DisplayMode]) -> Vec<DisplayInfo> {
    modes
        .iter()
        .enumerate()
        .map(|(index, mode)| DisplayInfo {
            index,
            width: mode.width,
            height: mode.height,
            refresh_hz: reported_refresh_rate(mode.frequency),
        })
        .collect()
}

/// The monitor at `index`, or an error naming how many exist.
pub fn pick_monitor<T: Copy>(monitors: &[T], index: usize) -> Result<T> {
    match monitors.get(index) {
        Some(m) => Ok(*m),
        None => bail!(
            "monitor {index} was requested, but this machine has {} (valid: 0..{})",
            monitors.len(),
            monitors.len().saturating_sub(1)
        ),
    }
}

/// The window to capture from `(handle, title)` candidates given topmost-first.
///
/// An exact title (case-insensitive) beats a substring match, so `"Notepad"` does not lose to
/// an earlier `"Notepad++ - notes"`; among equals the topmost wins. No match is an error, never
/// a fallback to the monitor: a recording of the wrong thing is worse than none.
pub fn pick_window<T: Copy>(candidates: &[(T, String)], needle: &str) -> Result<T> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        bail!("the window title to capture is empty");
    }
    let lower: Vec<String> = candidates.iter().map(|(_, t)| t.to_lowercase()).collect();
    let hit = lower
        .iter()
        .position(|t| *t == needle)
        .or_else(|| lower.iter().position(|t| t.contains(&needle)));
    match hit {
        Some(i) => Ok(candidates[i].0),
        None => bail!("no visible window has a title containing {needle:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_target_is_the_primary_monitor() {
        assert!(CaptureTarget::default().is_default());
        assert!(!CaptureTarget::from_config(1, "").is_default());
        assert!(!CaptureTarget::from_config(0, "Game").is_default());
    }

    #[test]
    fn blank_window_means_no_window() {
        assert_eq!(CaptureTarget::from_config(0, "  ").window, None);
        assert_eq!(
            CaptureTarget::from_config(0, " Game ").window.as_deref(),
            Some("Game")
        );
    }

    #[test]
    fn primary_is_index_zero_wherever_the_os_lists_it() {
        assert_eq!(ordered_monitors(2, &[1, 2, 3]), vec![2, 1, 3]);
        assert_eq!(ordered_monitors(1, &[1]), vec![1]);
    }

    #[test]
    fn out_of_range_monitor_names_the_count() {
        let err = pick_monitor(&[10, 20], 2).unwrap_err().to_string();
        assert!(err.contains("has 2"), "{err}");
        assert_eq!(pick_monitor(&[10, 20], 1).unwrap(), 20);
    }

    #[test]
    fn exact_title_beats_an_earlier_substring_and_case_is_ignored() {
        let w = vec![
            (1, "Notepad++ - notes".to_string()),
            (2, "NOTEPAD".to_string()),
        ];
        assert_eq!(pick_window(&w, "notepad").unwrap(), 2);
        assert_eq!(pick_window(&w, "notes").unwrap(), 1);
    }

    #[test]
    fn topmost_substring_wins_and_no_match_is_an_error() {
        let w = vec![(1, "Game A".to_string()), (2, "Game B".to_string())];
        assert_eq!(pick_window(&w, "game").unwrap(), 1);
        assert!(pick_window(&w, "other").is_err());
        assert!(pick_window(&w, "  ").is_err());
    }

    // --- Displays and their refresh rates ---

    #[test]
    fn a_rate_of_zero_or_one_is_not_a_rate() {
        // Windows answers 0 or 1 for a mode that inherits the display's default rate; neither is
        // a rate a person could pick from, so neither is reported as one.
        assert_eq!(reported_refresh_rate(0), None);
        assert_eq!(reported_refresh_rate(1), None);
    }

    #[test]
    fn the_rates_a_person_can_pick_from_are_reported_as_they_are() {
        assert_eq!(reported_refresh_rate(60), Some(60));
        assert_eq!(reported_refresh_rate(120), Some(120));
        assert_eq!(reported_refresh_rate(144), Some(144));
    }

    #[test]
    fn an_absurd_rate_is_unknown_rather_than_believed() {
        // A driver reporting thousands of Hz is reporting nonsense; an unknown rate is honest and
        // a wrong one is not.
        assert_eq!(reported_refresh_rate(5_000), None);
        assert_eq!(reported_refresh_rate(u32::MAX), None);
        // The ceiling itself is still a number, so it is reported.
        assert_eq!(
            reported_refresh_rate(MAX_PLAUSIBLE_REFRESH_HZ),
            Some(MAX_PLAUSIBLE_REFRESH_HZ)
        );
        // One above it is not.
        assert_eq!(reported_refresh_rate(MAX_PLAUSIBLE_REFRESH_HZ + 1), None);
    }

    #[test]
    fn the_displays_are_numbered_in_the_order_the_capture_target_indexes_them() {
        let modes = [
            DisplayMode {
                width: 3840,
                height: 2160,
                frequency: 120,
            },
            DisplayMode {
                width: 1920,
                height: 1080,
                frequency: 60,
            },
        ];
        let displays = indexed_displays(&modes);
        assert_eq!(displays.len(), 2);
        // The index is the position, which is what `[capture] monitor` means by it.
        assert_eq!(displays[0].index, 0);
        assert_eq!(displays[1].index, 1);
        assert_eq!(displays[0].width, 3840);
        assert_eq!(displays[0].height, 2160);
        assert_eq!(displays[0].refresh_hz, Some(120));
        assert_eq!(displays[1].refresh_hz, Some(60));
    }

    #[test]
    fn a_display_whose_rate_was_not_reported_stays_unknown() {
        // The mode is real; the rate was not reported, so the size is still worth having.
        let displays = indexed_displays(&[DisplayMode {
            width: 1920,
            height: 1080,
            frequency: 1,
        }]);
        assert_eq!(displays[0].width, 1920);
        assert_eq!(displays[0].refresh_hz, None);
    }

    #[test]
    fn no_displays_means_no_displays() {
        assert!(indexed_displays(&[]).is_empty());
    }
}
