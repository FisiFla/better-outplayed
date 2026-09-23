//! Clip triggers and local-only game event sources.
//!
//! # What is in this crate
//!
//! * [`Trigger`] and [`EventKind`] — the vocabulary the replay buffer and the clip index
//!   speak (spec §5.3, §5.5).
//! * [`hotkey`] — the global `RegisterHotKey` listener (spec §7.4), which needs no game.
//! * [`lol`] — the League of Legends Live Client Data poller (spec §7.1).
//! * [`wire`] — the HTTP/1.1 framing the two integrations share.
//!
//! # Both integrations are loopback-only, by construction
//!
//! The application performs no outbound network requests (spec §2, §7.3). That is not a
//! promise about behaviour, it is a property of the types:
//!
//! * the League client ([`lol::client::LoopbackClient`]) is address-pinned to an
//!   [`lol::Endpoint`], and an `Endpoint` can only be built for a loopback address;
//!
//! `tests/no_egress.rs` checks both statically over the tree, including that the one
//! place that relaxes TLS verification is the one file that cannot address anything but
//! this machine.
//!
//! # What an integration produces
//!
//! A source derives [`GameEvent`]s and hands them to a driver over an [`EventSink`]
//! channel. A driver (the CLI today) turns a highlight into a clip through the *same*
//! trigger path as the hotkey — [`EventKind::is_highlight`] is the only policy in the
//! way, and it exists so that a 30-round Counter-Strike match does not write 60 clips.

use std::fmt;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

pub mod hotkey;
pub mod lol;
pub mod wire;

/// A request to clip. Both manual and automatic paths funnel through this, so the
/// replay buffer has exactly one entry point (spec §5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    Hotkey,
    GameEvent { kind: EventKind, at: Instant },
}

/// What happened, in the vocabulary the integrations, the clip index and the session UI
/// share.
///
/// The first six are the kinds spec §7.1 names. The four after them are what the GSI
/// mapping (spec §7.2) needs in order to say "the bomb went down" without pretending that
/// it is a League objective; they are additive, because `events.kind` is TEXT and the
/// replay buffer never matches on a kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    Kill,
    Death,
    Assist,
    /// A game-level milestone: a League dragon/baron/herald/turret, or a Counter-Strike
    /// multi-kill or ace.
    Objective,
    GameStart,
    GameEnd,
    /// Counter-Strike: a new round's freezetime has begun.
    RoundStart,
    /// Counter-Strike: the round is over (`round.phase == "over"`).
    RoundEnd,
    BombPlanted,
    BombDefused,
}

impl EventKind {
    /// Every kind, for exhaustive tests and for a UI that wants to name them all.
    pub const ALL: [EventKind; 10] = [
        EventKind::Kill,
        EventKind::Death,
        EventKind::Assist,
        EventKind::Objective,
        EventKind::GameStart,
        EventKind::GameEnd,
        EventKind::RoundStart,
        EventKind::RoundEnd,
        EventKind::BombPlanted,
        EventKind::BombDefused,
    ];

    /// The string this kind is persisted as — `events.kind` (spec §5.5) and the `"kind"`
    /// a front-end reads back. Stable: the desktop shell's timeline renders it.
    pub fn as_tag(self) -> &'static str {
        match self {
            EventKind::Kill => "kill",
            EventKind::Death => "death",
            EventKind::Assist => "assist",
            EventKind::Objective => "objective",
            EventKind::GameStart => "game_start",
            EventKind::GameEnd => "game_end",
            EventKind::RoundStart => "round_start",
            EventKind::RoundEnd => "round_end",
            EventKind::BombPlanted => "bomb_planted",
            EventKind::BombDefused => "bomb_defused",
        }
    }

    /// The inverse of [`EventKind::as_tag`], for reading a row back. `None` for a tag this
    /// build does not know: a row written by a newer version is not an error.
    pub fn from_tag(tag: &str) -> Option<EventKind> {
        EventKind::ALL.into_iter().find(|k| k.as_tag() == tag)
    }

    /// Whether this kind is worth a clip on its own.
    ///
    /// Clipping every derived event is not the same as clipping every *interesting* one.
    /// A 30-round Counter-Strike match is ~60 round transitions, and a League game is a
    /// loading screen before it is anything else: a clip for each of those would fill the
    /// clips directory with footage nobody asked for, at `pre_seconds` apiece. So the
    /// vocabulary itself says which kinds are highlights — the moments a player would have
    /// reached for the hotkey for — and which are markers, recorded in `events` with no
    /// clip attached so the session timeline still shows them.
    ///
    /// `GameEnd` is a highlight on purpose: the last seconds of a match are exactly the
    /// footage people keep. `GameStart` is a loading screen. Round transitions are pacing,
    /// not events.
    ///
    /// This is the single place the policy lives. A user who disagrees changes one match
    /// arm here rather than hunting through a driver.
    pub fn is_highlight(self) -> bool {
        match self {
            EventKind::Kill
            | EventKind::Death
            | EventKind::Assist
            | EventKind::Objective
            | EventKind::BombPlanted
            | EventKind::BombDefused
            | EventKind::GameEnd => true,
            EventKind::GameStart | EventKind::RoundStart | EventKind::RoundEnd => false,
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_tag())
    }
}

/// Which integration derived an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// League of Legends, Live Client Data API (spec §7.1).
    Lol,
    /// Counter-Strike 2 / Dota 2, Game State Integration (spec §7.2).
    Gsi,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Lol => "lol",
            Source::Gsi => "gsi",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One event an integration derived, ready for the engine.
///
/// The `source` is carried beside the payload as well as inside it, deliberately: a log
/// line needs to name the integration without parsing JSON, and a reader of the `events`
/// table needs the row to explain itself. The payload is one line of JSON (compact) — the
/// `events.payload` column (spec §5.5), which holds the integration-specific detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameEvent {
    pub source: Source,
    pub kind: EventKind,
    pub payload: Option<String>,
}

impl GameEvent {
    /// Build an event whose payload is `detail`, rendered as one line of JSON. Rendering
    /// cannot fail for a `serde_json::Value`, so this needs no `Result` — and the value is
    /// produced by this crate, never echoed from a socket.
    pub fn new(source: Source, kind: EventKind, detail: serde_json::Value) -> Self {
        Self { source, kind, payload: Some(detail.to_string()) }
    }

    /// An event with no detail at all. Used by the tests; a real derivation always has
    /// something to say about what it saw.
    pub fn bare(source: Source, kind: EventKind) -> Self {
        Self { source, kind, payload: None }
    }

    /// Whether this event should become a clip (see [`EventKind::is_highlight`]).
    pub fn is_highlight(&self) -> bool {
        self.kind.is_highlight()
    }
}

/// Where an integration sends what it derived.
///
/// Unbounded on purpose: the volume is a handful of events per minute, and a bounded queue
/// would turn "the driver is busy taking the previous clip" into a dropped event — the one
/// thing an event source must not do.
pub type EventSink = Sender<GameEvent>;

/// Maps an instant onto the capture timeline, which is what the ledger indexes.
#[derive(Debug)]
pub struct CaptureClock {
    start: Instant,
}

impl CaptureClock {
    pub fn new() -> Self {
        Self { start: Instant::now() }
    }

    pub fn ms_at(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.start).as_millis() as u64
    }

    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

impl Default for CaptureClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_kinds_round_trip_through_their_tags() {
        for kind in EventKind::ALL {
            assert_eq!(
                EventKind::from_tag(kind.as_tag()),
                Some(kind),
                "{kind} must survive a round trip through the events table"
            );
            assert_eq!(kind.to_string(), kind.as_tag());
        }
        assert_eq!(EventKind::from_tag("round_won"), None, "an unknown tag is not an error");
    }

    #[test]
    fn the_tags_are_the_ones_the_events_table_holds() {
        // Pinned literally: these strings are in the database, and a rename would silently
        // orphan every row already written.
        assert_eq!(EventKind::Kill.as_tag(), "kill");
        assert_eq!(EventKind::Objective.as_tag(), "objective");
        assert_eq!(EventKind::GameEnd.as_tag(), "game_end");
        assert_eq!(EventKind::BombPlanted.as_tag(), "bomb_planted");
    }

    #[test]
    fn highlights_are_the_moments_a_hotkey_would_have_been_pressed_for() {
        for kind in [EventKind::Kill, EventKind::Death, EventKind::Assist, EventKind::Objective] {
            assert!(kind.is_highlight(), "{kind} is a highlight");
        }
        assert!(EventKind::BombPlanted.is_highlight());
        assert!(EventKind::BombDefused.is_highlight());
        assert!(
            EventKind::GameEnd.is_highlight(),
            "the last seconds of a match are worth keeping"
        );
        // The markers: recorded, never a clip on their own.
        for kind in [EventKind::GameStart, EventKind::RoundStart, EventKind::RoundEnd] {
            assert!(!kind.is_highlight(), "{kind} is a marker, not a clip");
        }
    }

    #[test]
    fn an_event_payload_is_one_line_of_json() {
        let event = GameEvent::new(
            Source::Lol,
            EventKind::Kill,
            serde_json::json!({ "source": "lol", "killer": "Ahri" }),
        );
        let payload = event.payload.expect("a derived event carries its detail");
        assert!(!payload.contains('\n'), "the payload column holds one line: {payload}");
        assert!(payload.contains("Ahri"), "and the detail: {payload}");
    }
}
