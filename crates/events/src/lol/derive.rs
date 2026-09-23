//! Snapshot → events, as a pure state machine.
//!
//! Nothing in this module has a socket, a clock or a log: it is
//! `previous knowledge + one document → events`, so every rule below is a unit test.
//! [`crate::lol::poller`] is what puts it on a network.
//!
//! # The attach policy, stated explicitly
//!
//! The Live Client serves the whole event log of the game so far, and a game is usually
//! well under way before localplay is started. Replaying that history would be an event
//! storm — dozens of clips at once, all of footage the user did not ask for. So:
//!
//! | transition | what is emitted |
//! |---|---|
//! | no game → game | **`GameStart` only.** Every event already in the document is adopted as seen (`adopted_in_progress_events` in the payload says how many), and none of them is emitted. |
//! | game → no game | **`GameEnd`**, once. Emitted from the *falling edge*, so a poll that finds no game, and every poll after it, is silent. |
//! | game → game | only events whose id has not been seen. |
//! | a fresh `GameStart` line appears mid-game | the client restarted without the endpoint going quiet: the tracker resets, adopts again and emits **`GameStart` only** — the same policy, because it is the same situation. |
//!
//! # Idempotence
//!
//! Events are identified by `EventID` (a monotonic per-game counter) and only unseen ids
//! are emitted, so polling the same document twice emits nothing the second time. A
//! document whose events carry no id (the client has been seen to omit it) falls back to a
//! synthetic key derived from the event's own content, which keeps idempotence without
//! inventing an ordering.
//!
//! # The mapping (spec §7.1)
//!
//! | `EventName` | kind |
//! |---|---|
//! | `ChampionKill` | `Kill` / `Death` / `Assist`, by comparing `KillerName` / `VictimName` / `Assisters` against the watched player's names |
//! | `DragonKill`, `BaronKill`, `HeraldKill`, `EpicMonsterKill`, `HordeKill`, `TurretKilled`, `InhibKilled`, `FirstBrick`, `FirstBlood`, `Ace`, `Multikill` | `Objective` |
//! | `GameStart`, `GameEnd` | handled by the state machine above, never by the table |
//! | anything else (`MinionsSpawning`, a future name) | nothing |
//!
//! A `ChampionKill` that involves nobody the document names as the watched player is
//! **not** emitted: a fight the user was not in is not a clip. When the document names no
//! player at all — the loading screen, or a patch that moved the field — there is nothing
//! to compare against, and the kill is emitted as `Kill` with the names in its payload
//! rather than being thrown away.

use crate::lol::payload::{GameSnapshot, StreamEvent};
use crate::{EventKind, GameEvent, Source};
use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// The state machine: what has been seen, and what is currently true.
///
/// Derives `Default` — "nothing seen, no game" — which is what a freshly started poller
/// knows.
#[derive(Debug, Default)]
pub struct Tracker {
    in_game: bool,
    /// Ids of the events this track has already accounted for. Cleared whenever the game
    /// changes, because the client numbers ids per game.
    seen: HashSet<EventKey>,
    /// The game clock at the last document that described a game, for the `GameEnd`
    /// payload's "how long did it run" line.
    last_game_time: f64,
    /// How many in-progress events the last attach adopted without emitting. Exposed for
    /// the poller's log line, which is where a user finds out why the first clip is not
    /// preceded by forty others.
    adopted_on_attach: u64,
    /// The map the last attach was for, for the poller's log line. Kept as a field rather
    /// than read back out of the payload the derivation just rendered.
    attached_map: String,
}

impl Tracker {
    /// Observe one poll and return what it implies.
    ///
    /// `observed` is `None` when the poll found no game — a 404, or a connection refusal.
    /// A poll that *failed* (a malformed body, a TLS error) must not be passed in as
    /// `None`: the caller keeps its state instead, which is why
    /// [`crate::lol::poller::LolPoller::poll_once`] decides that and this function does not
    /// see it.
    pub fn observe(&mut self, observed: Option<&GameSnapshot>) -> Vec<GameEvent> {
        let Some(snapshot) = observed.filter(|snapshot| snapshot.game_present()) else {
            return self.end_game();
        };

        if !self.in_game {
            return self.start_game(snapshot);
        }
        // The player quit and requeued faster than the endpoint stopped answering: this
        // document is a different game. Re-attach, exactly as if the endpoint had gone
        // quiet first — otherwise the new game's early events arrive as a burst of clips.
        if self.started_again(snapshot) {
            return self.start_game(snapshot);
        }

        let names = snapshot.player_names();
        let mut out = Vec::new();
        for entry in snapshot.event_stream() {
            let key = EventKey::of(entry);
            if self.seen.contains(&key) {
                continue;
            }
            match entry.event_name.as_str() {
                // The game's own `GameStart` line, seen for the first time. That happens
                // when the client begins including an event history it was not sending
                // before — the loading screen serves an empty list — so this is the game
                // already being watched, not a new one. Adopted in silence; a *different*
                // game is recognised by its clock, below.
                "GameStart" => {
                    self.seen.insert(key);
                }
                // The document sometimes carries the end of the game as an event. Treated
                // as the end of the game, so that the 404 which follows does not report it
                // a second time.
                "GameEnd" => {
                    self.in_game = false;
                    self.seen.clear();
                    return vec![end_event(snapshot.game_time())];
                }
                name => {
                    self.seen.insert(key);
                    if let Some(kind) = classify(name, entry, &names) {
                        out.push(stream_event(name, entry, kind, snapshot));
                    }
                }
            }
        }
        self.last_game_time = snapshot.game_time();
        out
    }

    /// Whether this document belongs to a *different* game than the one being tracked.
    ///
    /// The Live Client's `gameTime` counts up from a negative value during the loading
    /// screen, so a clock that has jumped backwards can only mean the client started
    /// another game — the endpoint never went quiet, so there is no `GameEnd` to see. Event
    /// ids restart with the game and cannot be used for this (`EventID` 0 is `GameStart` in
    /// every game). A document that omits the clock is not evidence of anything and never
    /// triggers this.
    fn started_again(&self, snapshot: &GameSnapshot) -> bool {
        /// Seconds of backwards movement that cannot be jitter within one game.
        const BACKWARDS_S: f64 = 5.0;
        let now = snapshot.game_time();
        now != 0.0
            && self.last_game_time != 0.0
            && now < self.last_game_time - BACKWARDS_S
    }

    /// Whether a game is being played, as of the last observation.
    pub fn in_game(&self) -> bool {
        self.in_game
    }

    /// How many in-progress events the last attach adopted without emitting.
    pub fn adopted_on_attach(&self) -> u64 {
        self.adopted_on_attach
    }

    /// The game mode and map of the last attach, for a driver's log line. Empty before the
    /// first attach.
    pub fn attached_map(&self) -> &str {
        &self.attached_map
    }

    /// No game (or a new game): emit `GameStart` and take the document's history as already
    /// seen.
    fn start_game(&mut self, snapshot: &GameSnapshot) -> Vec<GameEvent> {
        self.in_game = true;
        self.seen.clear();
        self.adopted_on_attach = self.adopt(snapshot);
        self.last_game_time = snapshot.game_time();
        self.attached_map = snapshot.game_data.map_name.trim().to_string();
        vec![start_event(snapshot, self.adopted_on_attach)]
    }

    /// The falling edge: a game was being played and the endpoint no longer serves one.
    fn end_game(&mut self) -> Vec<GameEvent> {
        if !self.in_game {
            return Vec::new();
        }
        self.in_game = false;
        self.seen.clear();
        self.adopted_on_attach = 0;
        self.attached_map.clear();
        let event = end_event(self.last_game_time);
        self.last_game_time = 0.0;
        vec![event]
    }

    /// Mark every event in the document as seen, returning how many that was.
    fn adopt(&mut self, snapshot: &GameSnapshot) -> u64 {
        let mut adopted = 0;
        for entry in snapshot.event_stream() {
            // Deduplicate: an id-less document can legitimately repeat a line, and the
            // count reported to the user should be the number of *events* adopted.
            if self.seen.insert(EventKey::of(entry)) {
                adopted += 1;
            }
        }
        adopted
    }
}

/// How an event is identified for the "have I seen this?" question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum EventKey {
    /// The client's own `EventID`, which is monotonic within a game.
    Id(i64),
    /// A document with no id: a content hash, so the same line is not emitted twice.
    Content(u64),
}

impl EventKey {
    fn of(entry: &StreamEvent) -> Self {
        if entry.event_id != 0 {
            return EventKey::Id(entry.event_id);
        }
        // FNV-1a over the fields that describe the event. Not a security hash: it only has
        // to be stable and collision-resistant enough for a few hundred events.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut feed = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= *byte as u64;
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        feed(entry.event_name.as_bytes());
        feed(b"\x1f");
        // Time to a tenth of a second: the client's clock has more digits than a payload
        // ever needs, and two events at the same tenth are the same event.
        feed(format!("{:.1}", entry.event_time).as_bytes());
        for name in [
            entry.killer_name.as_deref(),
            entry.victim_name.as_deref(),
            entry.monster_type.as_deref(),
            entry.monster_sub_type.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            feed(b"\x1f");
            feed(name.as_bytes());
        }
        for assister in &entry.assisters {
            feed(b"\x1f");
            feed(assister.as_bytes());
        }
        EventKey::Content(hash)
    }
}

/// The documented `EventName` → [`EventKind`] mapping (see the module docs).
fn classify(name: &str, entry: &StreamEvent, names: &[&str]) -> Option<EventKind> {
    match name {
        // A champion kill is about the watched player, when the document names one.
        "ChampionKill" => {
            if names.is_empty() {
                // Nothing to compare against: the kill is real, so it is reported, with
                // both names in the payload for whoever reads it later.
                return Some(EventKind::Kill);
            }
            if entry.killer_name.as_deref().is_some_and(|k| names.contains(&k)) {
                return Some(EventKind::Kill);
            }
            if entry.victim_name.as_deref().is_some_and(|v| names.contains(&v)) {
                return Some(EventKind::Death);
            }
            if entry.assisters.iter().any(|a| names.contains(&a.as_str())) {
                return Some(EventKind::Assist);
            }
            // Somebody else's kill: not a clip. The hotkey exists for those moments.
            None
        }
        // Game-level milestones count whoever did them: a baron is a baron.
        "DragonKill" | "BaronKill" | "HeraldKill" | "EpicMonsterKill" | "HordeKill"
        | "TurretKilled" | "InhibKilled" | "FirstBrick" | "FirstBlood" | "Ace" | "Multikill" => {
            Some(EventKind::Objective)
        }
        // The lifecycle is the state machine's business; the pacing events are not
        // clip-worthy, and an unknown name is a patch this build has not seen.
        _ => None,
    }
}

/// A per-event payload: what happened, plus the fields the Live Client gave us for it.
///
/// A serde_json object built field by field so that a key the document omitted is absent
/// from the payload rather than present and null — the `events.payload` column is read by
/// a human and by the session UI, and `"killer": null` is noise.
fn event_payload(source: &str, name: &str, entry: &StreamEvent, game_time: f64) -> Value {
    let mut map = Map::new();
    map.insert("source".into(), json!(source));
    map.insert("event".into(), json!(name));
    if entry.event_id != 0 {
        map.insert("event_id".into(), json!(entry.event_id));
    }
    map.insert("game_time_s".into(), json!(round_tenth(game_time)));
    if let Some(killer) = &entry.killer_name {
        map.insert("killer".into(), json!(killer));
    }
    if let Some(victim) = &entry.victim_name {
        map.insert("victim".into(), json!(victim));
    }
    if !entry.assisters.is_empty() {
        map.insert("assisters".into(), json!(entry.assisters));
    }
    if let Some(monster) = &entry.monster_type {
        map.insert("monster".into(), json!(monster));
    }
    if let Some(sub) = &entry.monster_sub_type {
        map.insert("monster_sub_type".into(), json!(sub));
    }
    if let Some(stolen) = &entry.stolen {
        // The client sends the string "True"/"False"; a boolean is what a reader wants.
        map.insert("stolen".into(), json!(stolen.eq_ignore_ascii_case("true")));
    }
    if let Some(streak) = entry.kill_streak {
        map.insert("kill_streak".into(), json!(streak));
    }
    Value::Object(map)
}

fn stream_event(
    name: &str,
    entry: &StreamEvent,
    kind: EventKind,
    snapshot: &GameSnapshot,
) -> GameEvent {
    GameEvent::new(Source::Lol, kind, event_payload("lol", name, entry, snapshot.game_time()))
}

/// The `GameStart` payload: which game, and how much of it was already over.
fn start_event(snapshot: &GameSnapshot, adopted: u64) -> GameEvent {
    let mut map = Map::new();
    map.insert("source".into(), json!("lol"));
    map.insert("event".into(), json!("GameStart"));
    map.insert("game_mode".into(), json!(snapshot.game_data.game_mode));
    map.insert("map".into(), json!(snapshot.game_data.map_name));
    map.insert("game_time_s".into(), json!(round_tenth(snapshot.game_time())));
    map.insert(
        "adopted_in_progress_events".into(),
        json!(adopted),
    );
    GameEvent::new(Source::Lol, EventKind::GameStart, Value::Object(map))
}

/// The `GameEnd` payload. The endpoint simply stops answering, so the reason is stated in
/// the record rather than left to be inferred from an absence.
fn end_event(last_game_time: f64) -> GameEvent {
    GameEvent::new(
        Source::Lol,
        EventKind::GameEnd,
        json!({
            "source": "lol",
            "event": "GameEnd",
            "reason": "the Live Client stopped serving game data",
            "last_game_time_s": round_tenth(last_game_time),
        }),
    )
}

/// Two decimal places is more than any of these values needs, and it keeps a payload
/// readable and comparable in tests.
fn round_tenth(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const IN_PROGRESS: &str = include_str!("../../tests/fixtures/lol_in_progress.json");
    const EVENTS_NEXT: &str = include_str!("../../tests/fixtures/lol_events_next.json");
    const SECOND_GAME: &str = include_str!("../../tests/fixtures/lol_second_game.json");
    const NO_IDENTITY: &str = include_str!("../../tests/fixtures/lol_no_identity.json");
    const NONE: &str = include_str!("../../tests/fixtures/lol_none.json");
    const EMPTY: &str = include_str!("../../tests/fixtures/lol_empty.json");

    fn snapshot(fixture: &str) -> Option<GameSnapshot> {
        crate::lol::payload::parse(fixture.as_bytes()).expect("the fixture parses")
    }

    fn kinds(events: &[GameEvent]) -> Vec<EventKind> {
        events.iter().map(|e| e.kind).collect()
    }

    fn payload_of(events: &[GameEvent], kind: EventKind) -> &str {
        events
            .iter()
            .find(|e| e.kind == kind)
            .and_then(|e| e.payload.as_deref())
            .expect("the event is present and carries a payload")
    }

    #[test]
    fn attaching_to_a_game_already_in_progress_reports_the_attach_and_nothing_else() {
        // The event storm this policy exists to prevent: the document already holds six
        // events, two of which are the watched player's kill and death.
        let mut tracker = Tracker::default();
        let first = tracker.observe(snapshot(IN_PROGRESS).as_ref());

        assert_eq!(kinds(&first), vec![EventKind::GameStart], "one event, not seven");
        assert_eq!(tracker.adopted_on_attach(), 6, "the whole history is adopted as seen");
        assert!(tracker.in_game());
        let payload = first[0].payload.as_deref().unwrap();
        assert!(payload.contains("\"adopted_in_progress_events\":6"), "got {payload}");
        assert!(payload.contains("\"map\":\"Map11\""), "got {payload}");
    }

    #[test]
    fn the_tracker_remembers_what_it_attached_to() {
        let mut tracker = Tracker::default();
        assert_eq!(tracker.attached_map(), "", "nothing attached yet");
        tracker.observe(snapshot(IN_PROGRESS).as_ref());
        assert_eq!(tracker.attached_map(), "Map11");
        tracker.observe(None);
        assert_eq!(tracker.attached_map(), "", "the game is over; there is no map");
    }

    #[test]
    fn the_same_document_twice_emits_nothing_the_second_time() {
        let mut tracker = Tracker::default();
        tracker.observe(snapshot(IN_PROGRESS).as_ref());
        // The poller will serve the same document for several seconds while a game is
        // quiet; every one of those polls must be a no-op.
        for _ in 0..5 {
            assert!(tracker.observe(snapshot(IN_PROGRESS).as_ref()).is_empty());
        }
    }

    #[test]
    fn every_event_of_a_full_history_is_derived_once_and_in_order() {
        // No attach here: the tracker starts on an empty document (a game whose payload
        // has arrived but which the tracker has not yet seen is the *attach* case above).
        // Reached instead by attaching to an empty game and then seeing the history grow.
        let mut tracker = Tracker::default();

        // 1. A game with no events yet: attach.
        let start = crate::lol::payload::parse(
            br#"{"gameData":{"gameTime":-20.0,"mapName":"Map11"}}"#,
        )
        .unwrap();
        assert_eq!(kinds(&tracker.observe(start.as_ref())), vec![EventKind::GameStart]);

        // 2. The full history arrives in one document.
        let full = tracker.observe(snapshot(IN_PROGRESS).as_ref());
        assert_eq!(
            kinds(&full),
            vec![
                EventKind::Kill,       // EventID 2, the player's kill
                EventKind::Death,      // EventID 3, the player died
                EventKind::Objective,  // EventID 5, a dragon
            ],
            "the player's kills/deaths and the objectives, in the document's order — and \
             nothing for EventID 4, which involved nobody the document names as the player"
        );
        assert_eq!(
            full.iter().map(|e| e.source).collect::<Vec<_>>(),
            vec![Source::Lol; 3]
        );

        // 3. MinionsSpawning (id 1) and GameStart (id 0) are adopted without a clip.
        let payload = payload_of(&full, EventKind::Kill);
        assert!(payload.contains("\"killer\":\"FisiFla#EUW\""), "got {payload}");
        assert!(payload.contains("\"victim\":\"ZedMain\""), "got {payload}");

        // 4. The same document again: nothing.
        assert!(tracker.observe(snapshot(IN_PROGRESS).as_ref()).is_empty());
    }

    #[test]
    fn one_new_event_between_two_polls_is_emitted_exactly_once() {
        let mut tracker = Tracker::default();
        tracker.observe(snapshot(IN_PROGRESS).as_ref());

        let next = tracker.observe(snapshot(EVENTS_NEXT).as_ref());
        assert_eq!(
            kinds(&next),
            vec![EventKind::Kill, EventKind::Objective],
            "exactly the two new lines (ids 6 and 7): the six already seen are not replayed"
        );
        assert!(
            payload_of(&next, EventKind::Objective).contains("\"monster\":\"BARON_NASHOR\""),
            "the incremental event carries its own detail"
        );
        assert!(payload_of(&next, EventKind::Objective).contains("\"stolen\":true"));

        assert!(tracker.observe(snapshot(EVENTS_NEXT).as_ref()).is_empty());
    }

    #[test]
    fn the_game_ending_emits_one_end_event_and_nothing_after_it() {
        let mut tracker = Tracker::default();
        tracker.observe(snapshot(IN_PROGRESS).as_ref());
        tracker.observe(snapshot(EVENTS_NEXT).as_ref());

        // The Live Client stops answering when the game ends: the poller reports that as
        // "no game".
        let ended = tracker.observe(None);
        assert_eq!(kinds(&ended), vec![EventKind::GameEnd]);
        assert!(!tracker.in_game());
        let payload = ended[0].payload.as_deref().unwrap();
        assert!(payload.contains("\"last_game_time_s\":730.2"), "got {payload}");

        // And every later no-game poll is silent.
        for _ in 0..3 {
            assert!(tracker.observe(None).is_empty(), "the falling edge fires once");
        }
    }

    #[test]
    fn a_null_payload_or_an_empty_document_is_no_game() {
        let mut tracker = Tracker::default();
        tracker.observe(snapshot(IN_PROGRESS).as_ref());

        // `null` is a documented way for the endpoint to say "no game"; an empty object is
        // a client that is up with nothing to report. Both end the game, once.
        assert_eq!(kinds(&tracker.observe(snapshot(NONE).as_ref())), vec![EventKind::GameEnd]);
        assert!(tracker.observe(snapshot(EMPTY).as_ref()).is_empty(), "already ended");
    }

    #[test]
    fn a_new_game_starting_mid_game_resets_the_tracker_instead_of_replaying_it() {
        // The abrupt re-attach: the player quit and requeued, and the endpoint never went
        // quiet, so the first thing the tracker sees of the new game is a fresh GameStart
        // line — with ids that restart at 0.
        let mut tracker = Tracker::default();
        tracker.observe(snapshot(IN_PROGRESS).as_ref());
        tracker.observe(snapshot(EVENTS_NEXT).as_ref());

        let second = tracker.observe(snapshot(SECOND_GAME).as_ref());
        assert_eq!(
            kinds(&second),
            vec![EventKind::GameStart],
            "the new game's history is adopted, not emitted"
        );
        assert_eq!(tracker.adopted_on_attach(), 2);
        assert!(tracker.in_game());
    }

    #[test]
    fn a_game_that_ends_and_a_new_one_that_starts_is_two_events() {
        let mut tracker = Tracker::default();
        assert_eq!(kinds(&tracker.observe(snapshot(IN_PROGRESS).as_ref())), vec![EventKind::GameStart]);
        assert_eq!(kinds(&tracker.observe(None)), vec![EventKind::GameEnd]);
        assert_eq!(
            kinds(&tracker.observe(snapshot(SECOND_GAME).as_ref())),
            vec![EventKind::GameStart],
            "the second game is a new attach, with its own adoption"
        );
    }

    #[test]
    fn without_a_named_player_a_champion_kill_is_reported_as_a_kill() {
        // The document names no player (the loading screen, or a patch that moved the
        // field). There is nothing to compare against, so a real kill is reported rather
        // than dropped — with both names in the payload.
        let mut tracker = Tracker::default();
        // Attach on a game that has happened to report no events yet …
        tracker.observe(
            crate::lol::payload::parse(
                br#"{"gameData":{"gameTime":-20.0,"mapName":"Map11"}}"#,
            )
            .unwrap()
            .as_ref(),
        );
        // … and then the document that names no player but does report a kill.
        let events = tracker.observe(snapshot(NO_IDENTITY).as_ref());
        assert_eq!(kinds(&events), vec![EventKind::Kill]);
        let payload = payload_of(&events, EventKind::Kill);
        assert!(payload.contains("\"killer\":\"Somebody\""), "got {payload}");
        assert!(payload.contains("\"victim\":\"Someone\""), "got {payload}");
    }

    #[test]
    fn an_own_kill_is_a_kill_a_shared_kill_is_an_assist_and_an_own_death_is_a_death() {
        // One document per case, so the classification is pinned to the rule rather than
        // to the fixture's ordering.
        let with_player = |event: &str| -> Vec<EventKind> {
            let body = format!(
                r#"{{"gameData":{{"gameTime":100.0,"mapName":"Map11"}},
                     "activePlayer":{{"summonerName":"FisiFla#EUW"}},
                     "events":{{"Events":[{event}]}}}}"#
            );
            let mut tracker = Tracker::default();
            // Attach first, on a document with no events at all, so that the event under
            // test is a new line rather than part of an adopted history.
            tracker.observe(
                crate::lol::payload::parse(
                    br#"{"gameData":{"gameTime":-20.0,"mapName":"Map11"},
                         "activePlayer":{"summonerName":"FisiFla#EUW"}}"#,
                )
                .unwrap()
                .as_ref(),
            );
            let events = tracker.observe(crate::lol::payload::parse(body.as_bytes()).unwrap().as_ref());
            kinds(&events)
        };

        assert_eq!(
            with_player(r#"{"EventID":2,"EventName":"ChampionKill","EventTime":100.0,"KillerName":"FisiFla#EUW","VictimName":"ZedMain"}"#),
            vec![EventKind::Kill]
        );
        assert_eq!(
            with_player(r#"{"EventID":2,"EventName":"ChampionKill","EventTime":100.0,"KillerName":"ZedMain","VictimName":"FisiFla#EUW"}"#),
            vec![EventKind::Death]
        );
        assert_eq!(
            with_player(r#"{"EventID":2,"EventName":"ChampionKill","EventTime":100.0,"KillerName":"ZedMain","VictimName":"EnemyTop","Assisters":["FisiFla#EUW"]}"#),
            vec![EventKind::Assist]
        );
        assert!(
            with_player(r#"{"EventID":2,"EventName":"ChampionKill","EventTime":100.0,"KillerName":"EnemyTop","VictimName":"EnemyJungle"}"#).is_empty(),
            "a fight the player was not in is not a clip"
        );
    }

    #[test]
    fn objectives_are_derived_for_every_milestone_the_client_names() {
        for name in [
            "DragonKill",
            "BaronKill",
            "HeraldKill",
            "EpicMonsterKill",
            "HordeKill",
            "TurretKilled",
            "InhibKilled",
            "FirstBrick",
            "FirstBlood",
            "Ace",
            "Multikill",
        ] {
            assert_eq!(
                classify(name, &StreamEvent::default(), &["me"]),
                Some(EventKind::Objective),
                "{name} is an objective"
            );
        }
        // Pacing and lifecycle events never come out of the table.
        for name in ["MinionsSpawning", "GameStart", "GameEnd", "SomethingFromAPatch"] {
            assert_eq!(classify(name, &StreamEvent::default(), &["me"]), None, "{name}");
        }
    }

    #[test]
    fn an_id_less_document_still_emits_an_event_only_once() {
        // The client has been seen to omit `EventID`. The fallback key is the event's own
        // content, so polling the same document twice is still a no-op.
        let body = br#"{"gameData":{"gameTime":100.0,"mapName":"Map11"},
                       "activePlayer":{"summonerName":"FisiFla#EUW"},
                       "events":{"Events":[{"EventName":"DragonKill","EventTime":99.3,"MonsterType":"DRAGON"}]}}"#;
        let document = crate::lol::payload::parse(body).unwrap();

        let mut tracker = Tracker::default();
        tracker.observe(
            crate::lol::payload::parse(
                br#"{"gameData":{"gameTime":-20.0,"mapName":"Map11"},
                     "activePlayer":{"summonerName":"FisiFla#EUW"}}"#,
            )
            .unwrap()
            .as_ref(),
        );
        assert_eq!(kinds(&tracker.observe(document.as_ref())), vec![EventKind::Objective]);
        assert!(tracker.observe(document.as_ref()).is_empty(), "the same line again");

        // A *different* id-less event is emitted: the key is content, not "any id-less
        // event".
        let other = crate::lol::payload::parse(
            br#"{"gameData":{"gameTime":110.0,"mapName":"Map11"},
                 "activePlayer":{"summonerName":"FisiFla#EUW"},
                 "events":{"Events":[{"EventName":"DragonKill","EventTime":99.3,"MonsterType":"DRAGON"},
                                     {"EventName":"BaronKill","EventTime":108.9,"MonsterType":"BARON_NASHOR"}]}}"#,
        )
        .unwrap();
        assert_eq!(kinds(&tracker.observe(other.as_ref())), vec![EventKind::Objective]);
    }

    #[test]
    fn a_game_end_event_inside_the_document_is_not_reported_twice() {
        // Some builds put the end of the game in the stream. It ends the game; the 404
        // that follows must not end it again.
        let body = br#"{"gameData":{"gameTime":2400.0,"mapName":"Map11"},
                       "activePlayer":{"summonerName":"FisiFla#EUW"},
                       "events":{"Events":[{"EventID":40,"EventName":"GameEnd","EventTime":2400.0}]}}"#;
        let document = crate::lol::payload::parse(body).unwrap();

        let mut tracker = Tracker::default();
        tracker.observe(snapshot(IN_PROGRESS).as_ref());
        assert_eq!(kinds(&tracker.observe(document.as_ref())), vec![EventKind::GameEnd]);
        assert!(!tracker.in_game());
        assert!(tracker.observe(None).is_empty(), "the endpoint going quiet is not a second end");
    }

    #[test]
    fn every_event_kind_this_tracker_can_emit_is_tagged_for_the_events_table() {
        // The kinds reach the database through `EventKind::as_tag`; this states that the
        // set the LoL derivation produces is a subset of the vocabulary.
        let mut tracker = Tracker::default();
        let mut emitted: Vec<EventKind> = Vec::new();
        emitted.extend(kinds(&tracker.observe(snapshot(IN_PROGRESS).as_ref())));
        emitted.extend(kinds(&tracker.observe(snapshot(EVENTS_NEXT).as_ref())));
        emitted.extend(kinds(&tracker.observe(None)));
        assert!(!emitted.is_empty());
        for kind in emitted {
            assert_eq!(EventKind::from_tag(kind.as_tag()), Some(kind));
        }
    }
}
