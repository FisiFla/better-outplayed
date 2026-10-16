//! Full Match splitting: turning a session's marker timeline into per-match segments.
//!
//! A session recording is one long file; the game integrations write `game_start` and
//! `game_end` markers onto its timeline (League: the Live Client appearing and going quiet;
//! Counter-Strike/Dota: the GSI phase). This module is the pure rule that pairs them into
//! [`MatchSegment`]s — the ranges a "Full Match" clip is cut from. It reads rows and returns
//! values; it never touches the database or the file, so every edge of the pairing is
//! testable without either.
//!
//! The pairing is deliberately forgiving about what a real session looks like, and explicit
//! about every inference it makes (see [`MatchSegment::start_inferred`] and
//! [`MatchSegment::complete`]), because a Full Match cut at a guessed boundary is a different
//! claim from one cut at a recorded one:
//!
//! * `game_start` … `game_end` — an ordinary, complete match.
//! * `game_start` … `game_start` — the first game never ended (the integration lost the game,
//!   or the player requeued faster than it noticed): it is closed at the next start and
//!   marked incomplete.
//! * `game_start` … end of session — the recording stopped mid-game: closed at the session's
//!   media length (or at its last marker when the length is unknown), incomplete.
//! * `game_end` with no open start — recording began mid-game: the match starts at the
//!   previous match's end (or `0`), `start_inferred`.

use crate::SessionEvent;

pub const GAME_START: &str = "game_start";
pub const GAME_END: &str = "game_end";

/// One match inside a session, on the session's media timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchSegment {
    /// Zero-based position among the session's matches.
    pub index: usize,
    pub start_ms: i64,
    pub end_ms: i64,
    /// Whether a `game_end` marker closed it. `false` means the end is a boundary this
    /// module chose (next start, end of footage), not one the game reported.
    pub complete: bool,
    /// Whether the start is a boundary this module chose (`0` or the previous match's end)
    /// because no `game_start` marker preceded the `game_end`.
    pub start_inferred: bool,
    /// The `game_start` payload (game mode, map), verbatim JSON text, when there was one.
    pub start_payload: Option<String>,
    /// The `game_end` payload verbatim. For League this carries the end-of-game scoreboard
    /// and the win/lose result; the desktop layer parses it, this crate does not know it.
    pub end_payload: Option<String>,
    /// Every marker that falls inside `[start_ms, end_ms]`, the boundary markers included.
    pub events: Vec<SessionEvent>,
}

/// Pair a session's markers into matches.
///
/// `events` must be in timeline order (as [`crate::Store::events_for_session`] returns
/// them). `media_length_ms` is the session file's length, `0` when unknown.
pub fn split_matches(events: &[SessionEvent], media_length_ms: i64) -> Vec<MatchSegment> {
    struct Open {
        start_ms: i64,
        inferred: bool,
        payload: Option<String>,
    }
    let mut out: Vec<MatchSegment> = Vec::new();
    let mut open: Option<Open> = None;
    let mut prev_end = 0i64;

    let close = |out: &mut Vec<MatchSegment>,
                 o: Open,
                 end_ms: i64,
                 complete: bool,
                 end_payload: Option<String>| {
        let end_ms = end_ms.max(o.start_ms);
        out.push(MatchSegment {
            index: out.len(),
            start_ms: o.start_ms,
            end_ms,
            complete,
            start_inferred: o.inferred,
            start_payload: o.payload,
            end_payload,
            events: Vec::new(),
        });
        end_ms
    };

    for e in events {
        match e.kind.as_str() {
            GAME_START => {
                if let Some(o) = open.take() {
                    prev_end = close(&mut out, o, e.offset_ms, false, None);
                }
                open = Some(Open { start_ms: e.offset_ms, inferred: false, payload: e.payload.clone() });
            }
            GAME_END => {
                let o = open.take().unwrap_or(Open {
                    start_ms: prev_end,
                    inferred: true,
                    payload: None,
                });
                prev_end = close(&mut out, o, e.offset_ms, true, e.payload.clone());
            }
            _ => {}
        }
    }
    if let Some(o) = open.take() {
        let last = events.last().map_or(o.start_ms, |e| e.offset_ms);
        let end = if media_length_ms > o.start_ms { media_length_ms } else { last };
        close(&mut out, o, end, false, None);
    }

    for m in &mut out {
        m.events = events
            .iter()
            .filter(|e| e.offset_ms >= m.start_ms && e.offset_ms <= m.end_ms)
            .cloned()
            .collect();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: i64, kind: &str, at: i64) -> SessionEvent {
        SessionEvent { id, kind: kind.into(), offset_ms: at, payload: None, clip_id: None }
    }

    #[test]
    fn a_session_with_no_game_markers_has_no_matches() {
        assert!(split_matches(&[], 60_000).is_empty());
        assert!(split_matches(&[ev(1, "bookmark", 5), ev(2, "kill", 9)], 60_000).is_empty());
    }

    #[test]
    fn a_start_and_an_end_make_one_complete_match_with_its_markers() {
        let events = [
            ev(1, "bookmark", 500),
            ev(2, GAME_START, 1_000),
            ev(3, "kill", 4_000),
            ev(4, GAME_END, 9_000),
            ev(5, "bookmark", 9_500),
        ];
        let m = split_matches(&events, 20_000);
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].start_ms, m[0].end_ms, m[0].complete, m[0].start_inferred), (1_000, 9_000, true, false));
        let ids: Vec<_> = m[0].events.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![2, 3, 4]);
    }

    #[test]
    fn two_back_to_back_games_split_in_two() {
        let events = [
            ev(1, GAME_START, 0),
            ev(2, GAME_END, 5_000),
            ev(3, GAME_START, 8_000),
            ev(4, GAME_END, 12_000),
        ];
        let m = split_matches(&events, 12_000);
        assert_eq!(m.iter().map(|s| (s.index, s.start_ms, s.end_ms)).collect::<Vec<_>>(), vec![(0, 0, 5_000), (1, 8_000, 12_000)]);
    }

    #[test]
    fn a_start_with_no_end_closes_at_the_next_start_and_is_incomplete() {
        let events = [ev(1, GAME_START, 1_000), ev(2, GAME_START, 6_000), ev(3, GAME_END, 9_000)];
        let m = split_matches(&events, 0);
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].end_ms, m[0].complete), (6_000, false));
        assert!(m[1].complete);
    }

    #[test]
    fn a_match_cut_off_by_the_end_of_the_recording_runs_to_the_media_length() {
        let events = [ev(1, GAME_START, 1_000), ev(2, "kill", 3_000)];
        let m = split_matches(&events, 30_000);
        assert_eq!((m[0].end_ms, m[0].complete), (30_000, false));
        // Length unknown: fall back to the last marker rather than inventing footage.
        let m = split_matches(&events, 0);
        assert_eq!(m[0].end_ms, 3_000);
    }

    #[test]
    fn an_end_with_no_start_begins_at_zero_or_the_previous_match_end() {
        let m = split_matches(&[ev(1, GAME_END, 4_000)], 10_000);
        assert_eq!((m[0].start_ms, m[0].start_inferred), (0, true));

        let events = [ev(1, GAME_START, 1_000), ev(2, GAME_END, 4_000), ev(3, GAME_END, 7_000)];
        let m = split_matches(&events, 10_000);
        assert_eq!((m[1].start_ms, m[1].end_ms, m[1].start_inferred), (4_000, 7_000, true));
    }

    #[test]
    fn payloads_ride_along_verbatim() {
        let mut s = ev(1, GAME_START, 0);
        s.payload = Some("{\"map\":\"Map11\"}".into());
        let mut e = ev(2, GAME_END, 100);
        e.payload = Some("{\"scoreboard\":[]}".into());
        let m = split_matches(&[s, e], 100);
        assert_eq!(m[0].start_payload.as_deref(), Some("{\"map\":\"Map11\"}"));
        assert_eq!(m[0].end_payload.as_deref(), Some("{\"scoreboard\":[]}"));
    }
}
