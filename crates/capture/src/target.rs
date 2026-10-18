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
}
