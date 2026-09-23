//! Deciding which scratch segments are finished being written.

use std::collections::BTreeSet;

/// Sequence numbers observed on disk that are complete.
///
/// ffmpeg appends to the newest segment, so a segment is only trusted once a
/// strictly later one exists. Pure function: the caller does the filesystem walk.
pub fn newly_complete(observed: &[u64], highest_known: Option<u64>) -> Vec<u64> {
    let unique: BTreeSet<u64> = observed.iter().copied().collect();
    let Some(&max) = unique.iter().next_back() else {
        return Vec::new();
    };
    unique
        .into_iter()
        .filter(|seq| *seq < max)
        .filter(|seq| highest_known.is_none_or(|known| *seq > known))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_segment_is_complete_only_once_a_later_one_exists() {
        // seq 3 exists but ffmpeg may still be appending to it.
        assert_eq!(newly_complete(&[0, 1, 2, 3], None), vec![0, 1, 2]);
    }

    #[test]
    fn does_not_re_emit_already_known_segments() {
        assert_eq!(newly_complete(&[0, 1, 2, 3], Some(1)), vec![2]);
    }

    #[test]
    fn nothing_is_complete_when_only_one_segment_exists() {
        assert_eq!(newly_complete(&[0], None), Vec::<u64>::new());
    }

    #[test]
    fn tolerates_out_of_order_and_duplicate_observations() {
        assert_eq!(newly_complete(&[2, 0, 2, 1], None), vec![0, 1]);
    }

    #[test]
    fn empty_directory_yields_nothing() {
        assert_eq!(newly_complete(&[], None), Vec::<u64>::new());
    }
}
