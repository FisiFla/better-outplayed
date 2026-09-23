//! The Valve GSI POST body, as typed Rust (spec §7.2).
//!
//! One document type covers Counter-Strike 2 / CS:GO and Dota 2. They overlap in the
//! `provider` and `map` blocks and differ everywhere else, and Valve's contract is that
//! **every block is optional**: the `.cfg` the user installs decides what is sent, and a
//! block that is not subscribed is simply absent. So every field here defaults, an unknown
//! key is ignored, and a payload localplay does not recognise still parses — it just
//! yields nothing.
//!
//! Two shape differences are worth calling out, because they are what a hand-written
//! parser gets wrong:
//!
//! * Dota 2's `player` block is an object in a player's own client and an **array** in a
//!   spectator's. [`PlayerBlock`] accepts both (the array's first element is used, and the
//!   choice is documented in [`PlayerBlock::first`]).
//! * A player's kills live in `player.match_stats` in Counter-Strike and directly on
//!   `player` in Dota 2, under the same names. [`Player::kills`], [`Player::deaths`] and
//!   [`Player::assists`] answer for both.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Parse a GSI POST body.
///
/// The body must be a JSON **object**. That is not pedantry: `serde`'s derive will happily
/// read a struct out of a JSON *array* — an empty `[]` becomes a document with every field
/// defaulted — because it supports formats that have no maps. Left alone, that would turn a
/// nonsense body into "a document that says nothing", which the listener would accept and
/// answer `200` to. So the shape is checked, once, at the edge.
pub fn parse(body: &[u8]) -> Result<GsiSnapshot, serde_json::Error> {
    let object: std::collections::BTreeMap<String, serde_json::Value> =
        serde_json::from_slice(body)?;
    serde_json::from_value(serde_json::Value::Object(object.into_iter().collect()))
}

/// One GSI document.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct GsiSnapshot {
    /// Whatever the installed `.cfg` declared under `"auth"`, echoed back verbatim by the
    /// game. This is where the token arrives — see [`crate::gsi::token_matches`].
    pub auth: BTreeMap<String, String>,
    pub provider: Provider,
    pub map: Map,
    pub round: Round,
    pub player: PlayerBlock,
    pub bomb: Bomb,
    pub hero: Hero,
}

/// `provider` — which game is talking.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Provider {
    pub name: String,
    pub appid: u32,
    pub version: u32,
    pub steamid: Option<String>,
    pub timestamp: u64,
}

/// `map` — the map, the phase of the match, and (in Dota 2) the rules state.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Map {
    pub mode: String,
    pub name: String,
    /// Counter-Strike: `warmup` | `live` | `intermission` | `gameover`.
    pub phase: String,
    /// Counter-Strike: the round number, 1-based.
    pub round: u32,
    pub team_ct: Team,
    pub team_t: Team,
    /// Dota 2: `DOTA_GAMERULES_STATE_*`.
    pub game_state: String,
    /// Dota 2: the match clock in seconds.
    pub game_time: i64,
    pub match_id: Option<String>,
    pub radiant_score: Option<u32>,
    pub dire_score: Option<u32>,
    pub win_team: Option<String>,
}

/// A team's score.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Team {
    pub score: u32,
    pub name: String,
}

/// `round` — Counter-Strike's phase and bomb state for the current round.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Round {
    /// `freezetime` | `live` | `over`.
    pub phase: String,
    /// `planted` | `defused` | `exploded` (absent when nothing happened).
    pub bomb: String,
    pub win_team: Option<String>,
}

/// `bomb` — the detailed bomb state, which the wiki documents as richer than `round.bomb`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Bomb {
    /// `carried` | `planting` | `planted` | `defusing` | `defused` | `dropped` | `exploded`.
    pub state: String,
    pub countdown: Option<String>,
    pub player: Option<String>,
}

/// `player` — either an object or an array; see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PlayerBlock {
    /// Absent: the `player` block is not subscribed, or the game has not sent one yet.
    #[default]
    Missing,
    One(Box<Player>),
    Many(Vec<Player>),
}

impl PlayerBlock {
    /// The block's player, if it has one.
    ///
    /// In spectator mode Dota 2 sends one entry per player slot and there is no way from
    /// the payload alone to know which slot is being watched; the first is used, and the
    /// kill/death derivation is guarded by `steamid` (see [`crate::gsi::derive`]), so a
    /// wrong guess cannot produce a burst of events — it produces none.
    pub fn first(&self) -> Option<&Player> {
        match self {
            PlayerBlock::Missing => None,
            PlayerBlock::One(player) => Some(player),
            PlayerBlock::Many(players) => players.first(),
        }
    }

    /// The same player, mutably. Used by the derivation's tests, which edit one field of a
    /// fixture rather than keeping a near-copy of the whole document for each case.
    pub fn first_mut(&mut self) -> Option<&mut Player> {
        match self {
            PlayerBlock::Missing => None,
            PlayerBlock::One(player) => Some(player),
            PlayerBlock::Many(players) => players.first_mut(),
        }
    }
}

/// `player` — who is playing, and their scoreboard.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Player {
    pub steamid: Option<String>,
    pub name: Option<String>,
    pub team: Option<String>,
    pub state: PlayerState,
    pub match_stats: MatchStats,
    /// Dota 2 puts the scoreboard directly on the player block.
    pub kills: Option<u32>,
    pub deaths: Option<u32>,
    pub assists: Option<u32>,
    pub gold: Option<u32>,
    pub net_worth: Option<u32>,
}

impl Player {
    /// Kills this player has, from whichever block the game puts them in. `None` when the
    /// document does not say — which is different from zero, and the difference is what
    /// keeps a missing block from looking like a score reset.
    pub fn kills(&self) -> Option<u32> {
        self.match_stats.kills.or(self.kills)
    }

    pub fn deaths(&self) -> Option<u32> {
        self.match_stats.deaths.or(self.deaths)
    }

    pub fn assists(&self) -> Option<u32> {
        self.match_stats.assists.or(self.assists)
    }
}

/// `player.state` — Counter-Strike's live per-round state.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct PlayerState {
    pub health: Option<i32>,
    pub armor: Option<i32>,
    pub round_kills: Option<u32>,
    pub round_killhs: Option<u32>,
}

/// `player.match_stats` — Counter-Strike's per-match totals.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct MatchStats {
    pub kills: Option<u32>,
    pub assists: Option<u32>,
    pub deaths: Option<u32>,
    pub mvps: Option<u32>,
    pub score: Option<u32>,
}

/// `hero` — Dota 2's hero block, kept for the payload's detail.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Hero {
    pub id: Option<u32>,
    pub name: String,
    pub level: Option<u32>,
    pub alive: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const CS2_COMPETITIVE: &str = include_str!("../../tests/fixtures/gsi_cs2_competitive.json");
    const CS2_MENU: &str = include_str!("../../tests/fixtures/gsi_cs2_menu.json");
    const DOTA_IN_GAME: &str = include_str!("../../tests/fixtures/gsi_dota_in_game.json");
    const DOTA_OBSERVER: &str = include_str!("../../tests/fixtures/gsi_dota_observer.json");

    #[test]
    fn parses_a_counter_strike_competitive_match() {
        let gsi = parse(CS2_COMPETITIVE.as_bytes()).expect("the fixture parses");
        assert_eq!(gsi.provider.appid, 730);
        assert_eq!(gsi.provider.name, "Counter-Strike: Global Offensive");
        assert_eq!(gsi.map.name, "de_dust2");
        assert_eq!(gsi.map.phase, "live");
        assert_eq!(gsi.map.round, 5);
        assert_eq!(gsi.map.team_ct.score, 3);
        assert_eq!(gsi.map.team_t.score, 1);
        assert_eq!(gsi.round.phase, "live");
        assert_eq!(gsi.bomb.state, "planted");
        assert_eq!(gsi.player.first().unwrap().steamid.as_deref(), Some("76561198000000001"));
        assert_eq!(gsi.player.first().unwrap().kills(), Some(12));
        assert_eq!(gsi.player.first().unwrap().deaths(), Some(5));
        assert_eq!(gsi.player.first().unwrap().assists(), Some(3));
    }

    #[test]
    fn parses_the_main_menu_where_nothing_is_subscribed() {
        let gsi = parse(CS2_MENU.as_bytes()).expect("the fixture parses");
        assert_eq!(gsi.map.name, "");
        assert_eq!(gsi.map.phase, "");
        assert!(gsi.player.first().is_none());
        assert_eq!(gsi.player, PlayerBlock::Missing);
    }

    #[test]
    fn parses_a_dota_match_whose_scoreboard_sits_on_the_player_block() {
        let gsi = parse(DOTA_IN_GAME.as_bytes()).expect("the fixture parses");
        assert_eq!(gsi.provider.appid, 570);
        assert_eq!(gsi.map.game_state, "DOTA_GAMERULES_STATE_GAME_IN_PROGRESS");
        assert_eq!(gsi.map.game_time, 1240);
        let player = gsi.player.first().expect("a single player block");
        assert_eq!(player.kills(), Some(4), "Dota puts the scoreboard on `player`");
        assert_eq!(player.deaths(), Some(2));
        assert_eq!(player.assists(), Some(6));
        assert_eq!(gsi.hero.name, "npc_dota_hero_queenofpain");
    }

    #[test]
    fn accepts_an_observer_payload_whose_player_block_is_an_array() {
        let gsi = parse(DOTA_OBSERVER.as_bytes()).expect("the fixture parses");
        match &gsi.player {
            PlayerBlock::Many(players) => assert_eq!(players.len(), 2),
            other => panic!("expected the observer shape, got {other:?}"),
        }
        assert_eq!(gsi.player.first().unwrap().steamid.as_deref(), Some("76561198000000009"));
    }

    #[test]
    fn a_missing_field_is_not_a_parse_error_and_a_wrong_type_is() {
        // Every block optional: the smallest document the game could send.
        let minimal = parse(br#"{"provider":{"appid":730,"name":"cs"}}"#).unwrap();
        assert_eq!(minimal.map.name, "");
        assert_eq!(minimal.player, PlayerBlock::Missing);

        // A wrong type is an error, not a silently zeroed field: a number where the
        // document says the score is, and a block that is neither object nor array.
        assert!(parse(br#"{"map":{"round":"five"}}"#).is_err());
        assert!(parse(br#"{"player":"nobody"}"#).is_err());
        assert!(parse(b"not json").is_err());
        assert!(parse(b"").is_err());
    }

    #[test]
    fn the_auth_block_is_an_arbitrary_string_map() {
        let gsi = parse(br#"{"auth":{"token":"abc","kind":"local"}}"#).unwrap();
        assert_eq!(gsi.auth.get("token").map(String::as_str), Some("abc"));
        assert_eq!(gsi.auth.get("kind").map(String::as_str), Some("local"));
        assert_eq!(gsi.auth.get("nothing").map(String::as_str), None);
    }

    #[test]
    fn a_kill_count_that_is_absent_is_not_zero() {
        // The tri-state matters: a payload without `player_match_stats` must not look like
        // a player whose score went back to zero.
        let player = Player::default();
        assert_eq!(player.kills(), None);
        assert_eq!(player.deaths(), None);
        assert_eq!(player.assists(), None);

        let cs2 = Player {
            match_stats: MatchStats { kills: Some(0), ..Default::default() },
            ..Default::default()
        };
        assert_eq!(cs2.kills(), Some(0), "a real zero is a value");
    }
}
