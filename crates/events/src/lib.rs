//! Clip triggers and local-only game event sources.
//!
//! * [`Trigger`] and [`EventKind`]: the vocabulary the replay buffer and the clip index speak.
//! * [`hotkey`]: the global `RegisterHotKey` listener, which needs no game.
//! * [`lol`]: the League of Legends Live Client Data poller.
//! * [`gsi`]: the CS2 / Dota 2 Game State Integration listener.
//! * [`process`]: is a watched game running? A process-list snapshot for titles that publish
//!   nothing, Riot's Live Client Data API for the one that does. Off by default
//!   (`games.auto_record = false`).
//! * [`wire`]: the HTTP/1.1 framing the two integrations share.
//!
//! # Loopback-only, by construction
//!
//! The application makes no outbound network requests, as a property of the types: the League
//! client ([`lol::client::LoopbackClient`]) is pinned to an [`lol::Endpoint`], which can only be
//! built for a loopback address, and the GSI listener binds `127.0.0.1` ([`gsi::bind`]) and
//! refuses any other. `tests/no_egress.rs` checks both statically, including that the one file
//! relaxing TLS verification cannot address anything but this machine.
//!
//! # What an integration produces
//!
//! A source derives [`GameEvent`]s and hands them to a driver over an [`EventSink`] channel. A
//! driver turns a highlight into a clip through the same trigger path as the hotkey;
//! [`EventKind::is_highlight`] is the only policy in the way, so a 30-round Counter-Strike
//! match does not write 60 clips.

// Test bodies may unwrap and may chunk by a constant; production code may do neither.
// Scoped to the test build of this crate, so it cannot hide either in shipped code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks))]

use std::fmt;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

pub mod gsi;
pub mod hotkey;
pub mod lol;
pub mod process;
pub mod wire;

// The GSI listener's configuration, at the crate root as well as under `gsi`: a caller
// that serves two games needs the binding type, and the module path is long enough to be
// worth a shorter name.
pub use gsi::{GsiConfig, GsiGameBinding, GsiGameInfo, GsiListener};

/// A request to clip. Manual and automatic paths both funnel through this, so the replay
/// buffer has one entry point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    Hotkey,
    GameEvent { kind: EventKind, at: Instant },
}

/// What happened, in the vocabulary the integrations, the clip index and the session UI share.
///
/// The GSI kinds let the mapping say "the bomb went down" without pretending it is a League
/// objective. Kinds are additive: `events.kind` is TEXT and the replay buffer never matches on
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    Kill,
    Death,
    Assist,
    /// A game-level milestone with no sub-kind of its own (League first blood, ace, multi-kill or
    /// void grub; Counter-Strike multi-kill or ace). Also the group tag `mute_clips =
    /// ["objective"]` uses for every objective kind (see [`EventKind::is_objective`]).
    Objective,
    /// A League dragon taken (any type; the type is in the payload).
    Dragon,
    /// A League baron taken.
    Baron,
    /// A League rift herald taken.
    Herald,
    /// A League turret destroyed (or the first brick).
    Turret,
    /// A League inhibitor destroyed.
    Inhibitor,
    GameStart,
    GameEnd,
    /// Counter-Strike: a new round's freezetime has begun.
    RoundStart,
    /// Counter-Strike: the round is over (`round.phase == "over"`).
    RoundEnd,
    BombPlanted,
    BombDefused,
    /// The player pressed the bookmark hotkey: a marker on the session timeline, no clip.
    Bookmark,
}

impl EventKind {
    /// Every kind, for exhaustive tests and for a UI that wants to name them all.
    pub const ALL: [EventKind; 16] = [
        EventKind::Kill,
        EventKind::Death,
        EventKind::Assist,
        EventKind::Objective,
        EventKind::Dragon,
        EventKind::Baron,
        EventKind::Herald,
        EventKind::Turret,
        EventKind::Inhibitor,
        EventKind::GameStart,
        EventKind::GameEnd,
        EventKind::RoundStart,
        EventKind::RoundEnd,
        EventKind::BombPlanted,
        EventKind::BombDefused,
        EventKind::Bookmark,
    ];

    /// The string this kind is persisted as (`events.kind`, and the `"kind"` a front-end reads
    /// back). Stable: the desktop timeline renders it.
    pub fn as_tag(self) -> &'static str {
        match self {
            EventKind::Kill => "kill",
            EventKind::Death => "death",
            EventKind::Assist => "assist",
            EventKind::Objective => "objective",
            EventKind::Dragon => "dragon",
            EventKind::Baron => "baron",
            EventKind::Herald => "herald",
            EventKind::Turret => "turret",
            EventKind::Inhibitor => "inhibitor",
            EventKind::GameStart => "game_start",
            EventKind::GameEnd => "game_end",
            EventKind::RoundStart => "round_start",
            EventKind::RoundEnd => "round_end",
            EventKind::BombPlanted => "bomb_planted",
            EventKind::BombDefused => "bomb_defused",
            EventKind::Bookmark => "bookmark",
        }
    }

    /// The inverse of [`EventKind::as_tag`]. `None` for a tag this build does not know: a row
    /// written by a newer version is not an error.
    pub fn from_tag(tag: &str) -> Option<EventKind> {
        EventKind::ALL.into_iter().find(|k| k.as_tag() == tag)
    }

    /// Whether this is [`EventKind::Objective`] or one of its sub-kinds. `mute_clips` treats the
    /// tag `"objective"` as the whole group, so muting objectives before the sub-kinds existed
    /// still mutes every dragon and turret.
    pub fn is_objective(self) -> bool {
        matches!(
            self,
            EventKind::Objective
                | EventKind::Dragon
                | EventKind::Baron
                | EventKind::Herald
                | EventKind::Turret
                | EventKind::Inhibitor
        )
    }

    /// Whether this kind is worth a clip on its own.
    ///
    /// A 30-round Counter-Strike match is ~60 round transitions and a League game starts with a
    /// loading screen; clipping each would fill the clips directory with footage nobody asked
    /// for. Highlights are the moments a player would have hit the hotkey for; the rest are
    /// markers, recorded in `events` with no clip so the session timeline still shows them.
    ///
    /// `GameEnd` is a highlight on purpose (the last seconds of a match are what people keep);
    /// `GameStart` and round transitions are not. This is the single place the policy lives.
    pub fn is_highlight(self) -> bool {
        match self {
            EventKind::Kill
            | EventKind::Death
            | EventKind::Assist
            | EventKind::Objective
            | EventKind::Dragon
            | EventKind::Baron
            | EventKind::Herald
            | EventKind::Turret
            | EventKind::Inhibitor
            | EventKind::BombPlanted
            | EventKind::BombDefused
            | EventKind::GameEnd => true,
            EventKind::GameStart
            | EventKind::RoundStart
            | EventKind::RoundEnd
            | EventKind::Bookmark => false,
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
    /// League of Legends, Live Client Data API
    Lol,
    /// Counter-Strike 2 / Dota 2, Game State Integration
    Gsi,
    /// The player, with a hotkey (a bookmark) — not an integration at all.
    Manual,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Lol => "lol",
            Source::Gsi => "gsi",
            Source::Manual => "manual",
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
/// `source` is carried beside the payload as well as inside it: a log line names the
/// integration without parsing JSON, and a reader of the `events` table gets a self-explaining
/// row. The payload is one compact line of JSON, the integration-specific detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameEvent {
    pub source: Source,
    pub kind: EventKind,
    pub payload: Option<String>,
}

impl GameEvent {
    /// Build an event whose payload is `detail`, rendered as one line of JSON. Rendering a
    /// `serde_json::Value` cannot fail, and the value is produced here, never echoed from a socket.
    pub fn new(source: Source, kind: EventKind, detail: serde_json::Value) -> Self {
        Self {
            source,
            kind,
            payload: Some(detail.to_string()),
        }
    }

    /// An event with no detail, for tests; a real derivation always has something to say.
    pub fn bare(source: Source, kind: EventKind) -> Self {
        Self {
            source,
            kind,
            payload: None,
        }
    }

    /// Whether this event should become a clip (see [`EventKind::is_highlight`]).
    pub fn is_highlight(&self) -> bool {
        self.kind.is_highlight()
    }
}

/// Where an integration sends what it derived.
///
/// Unbounded on purpose: the volume is a handful of events a minute, and a bounded queue would
/// turn "the driver is busy taking the previous clip" into a dropped event, which an event
/// source must never do.
pub type EventSink = Sender<GameEvent>;

/// Maps an instant onto the capture timeline the ledger indexes.
#[derive(Debug)]
pub struct CaptureClock {
    start: Instant,
}

impl CaptureClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
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
        assert_eq!(
            EventKind::from_tag("round_won"),
            None,
            "an unknown tag is not an error"
        );
    }

    #[test]
    fn the_tags_are_the_ones_the_events_table_holds() {
        // Pinned literally: these strings are in the database, and a rename would orphan every row
        // already written.
        assert_eq!(EventKind::Kill.as_tag(), "kill");
        assert_eq!(EventKind::Objective.as_tag(), "objective");
        assert_eq!(EventKind::Dragon.as_tag(), "dragon");
        assert_eq!(EventKind::Baron.as_tag(), "baron");
        assert_eq!(EventKind::Herald.as_tag(), "herald");
        assert_eq!(EventKind::Turret.as_tag(), "turret");
        assert_eq!(EventKind::Inhibitor.as_tag(), "inhibitor");
        assert_eq!(EventKind::GameEnd.as_tag(), "game_end");
        assert_eq!(EventKind::BombPlanted.as_tag(), "bomb_planted");
    }

    #[test]
    fn highlights_are_the_moments_a_hotkey_would_have_been_pressed_for() {
        for kind in [
            EventKind::Kill,
            EventKind::Death,
            EventKind::Assist,
            EventKind::Objective,
        ] {
            assert!(kind.is_highlight(), "{kind} is a highlight");
        }
        for kind in EventKind::ALL.into_iter().filter(|k| k.is_objective()) {
            assert!(
                kind.is_highlight(),
                "the objective sub-kind {kind} is a highlight"
            );
        }
        assert!(EventKind::BombPlanted.is_highlight());
        assert!(EventKind::BombDefused.is_highlight());
        assert!(
            EventKind::GameEnd.is_highlight(),
            "the last seconds of a match are worth keeping"
        );
        // The markers: recorded, never a clip on their own.
        for kind in [
            EventKind::GameStart,
            EventKind::RoundStart,
            EventKind::RoundEnd,
        ] {
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
        assert!(
            !payload.contains('\n'),
            "the payload column holds one line: {payload}"
        );
        assert!(payload.contains("Ahri"), "and the detail: {payload}");
    }
}
