//! The GSI listener over real loopback sockets (spec §7.2).
//!
//! The listener is started for real — a bound socket, the real request gate, the real token
//! comparison, the real derivation — and driven by a hand-written HTTP client in this file.
//! Nothing here is a mock of the listener; the only stand-in is the *game*, because no CS2 or
//! Dota 2 client was run (see `docs/plans/2026-09-23-localplay-phase-4-integrations.md`).
//!
//! Every socket is on `127.0.0.1`, and every payload is a canned fixture. The tests assert
//! four things the spec calls out explicitly:
//!
//! * a POST carrying the configured token is accepted and yields the expected events;
//! * a POST with a wrong token, or with no auth block at all, is rejected and yields
//!   nothing (fail closed);
//! * a malformed body yields nothing, does not panic, and does not stop the listener;
//! * the listener is bound to loopback, and refuses to bind anything else.

use localplay_events::gsi::{self, GsiConfig, GsiListener};
use localplay_events::{EventKind, GameEvent};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

const CS2_COMPETITIVE: &str = include_str!("fixtures/gsi_cs2_competitive.json");
const CS2_MENU: &str = include_str!("fixtures/gsi_cs2_menu.json");
const DOTA_IN_GAME: &str = include_str!("fixtures/gsi_dota_in_game.json");

/// The token the fixtures carry. Not a secret: it is in the fixtures on purpose so the
/// positive path can be exercised, and every rejection case posts something else.
const TOKEN: &str = "fixture-token-not-a-secret";

/// A listener plus the channel its events arrive on.
struct Harness {
    listener: GsiListener,
    events: Receiver<GameEvent>,
}

impl Harness {
    fn start() -> Self {
        let (sink, events) = channel();
        let listener =
            gsi::spawn(GsiConfig::new(0, TOKEN), sink).expect("the listener starts on a free port");
        Self { listener, events }
    }

    fn addr(&self) -> SocketAddr {
        self.listener.local_addr()
    }

    /// Every event the listener derived, within a short window.
    fn drained(&self) -> Vec<GameEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.events.recv_timeout(Duration::from_millis(400)) {
            out.push(event);
        }
        out
    }
}

fn kinds(events: &[GameEvent]) -> Vec<EventKind> {
    events.iter().map(|e| e.kind).collect()
}

/// Send raw bytes and read whatever comes back.
fn raw_request(addr: SocketAddr, request: &[u8]) -> String {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .expect("loopback connects");
    stream.set_read_timeout(Some(Duration::from_secs(3))).expect("a read timeout");
    stream.write_all(request).expect("the request is sent");
    stream.flush().expect("flushed");
    let mut raw = Vec::new();
    // The listener answers and closes, so reading to the end terminates.
    stream.read_to_end(&mut raw).expect("the response is read");
    String::from_utf8_lossy(&raw).to_string()
}

/// A well-formed POST to `path` with `body`.
fn post(addr: SocketAddr, path: &str, body: &str) -> String {
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    raw_request(addr, request.as_bytes())
}

fn status_of(response: &str) -> u16 {
    response
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {response:?}"))
}

/// A document with a token: the fixture, with its token replaced.
fn with_token(fixture: &str, token: &str) -> String {
    fixture.replace(TOKEN, token)
}

/// A document with no `auth` block at all — what a cfg file without one produces.
fn without_auth(fixture: &str) -> String {
    let start = fixture.find("\"auth\"").expect("the fixture has an auth block");
    let end = fixture[start..].find("},").expect("the block ends") + start + 2;
    fixture.replace(&fixture[start..end], "")
}

#[test]
fn the_listener_is_bound_to_loopback_and_answers_on_the_configured_path() {
    let harness = Harness::start();
    let addr = harness.addr();
    assert!(addr.ip().is_loopback(), "bound to {addr}");
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    assert_ne!(addr.port(), 0, "the OS assigned a real port");

    // A wrong path, and a method that is not POST, are refused by status.
    assert_eq!(status_of(&post(addr, "/", CS2_COMPETITIVE)), 404);
    assert_eq!(status_of(&post(addr, "/liveclientdata/allgamedata", CS2_COMPETITIVE)), 404);
    let get = format!("GET /gsi HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    assert_eq!(status_of(&raw_request(addr, get.as_bytes())), 405);
    assert!(harness.drained().is_empty(), "nothing was derived from any of those");
}

#[test]
fn a_post_with_the_configured_token_is_accepted_and_derives_the_expected_events() {
    let harness = Harness::start();
    let addr = harness.addr();

    // 1. The first payload of a match: an attach. The scoreboard on it (12 kills, 5 deaths,
    //    3 assists) is adopted, not reported.
    let response = post(addr, "/gsi", CS2_COMPETITIVE);
    assert_eq!(status_of(&response), 200, "the token is the configured one: {response}");
    let attach = harness.drained();
    assert_eq!(kinds(&attach), vec![EventKind::GameStart]);
    assert_eq!(attach[0].source.as_str(), "gsi");
    assert!(
        attach[0].payload.as_deref().unwrap_or("").contains("\"map\":\"de_dust2\""),
        "the attach records which map: {:?}",
        attach[0].payload
    );

    // 2. A heartbeat that repeats the state exactly: nothing.
    assert_eq!(status_of(&post(addr, "/gsi", CS2_COMPETITIVE)), 200);
    assert!(harness.drained().is_empty(), "a repeated payload is not an event");

    // 3. Round 5 ends, and the next round begins with a kill for the player.
    let mut next: serde_json::Value =
        serde_json::from_str(CS2_COMPETITIVE).expect("the fixture is JSON");
    next["round"]["phase"] = serde_json::json!("over");
    next["round"]["win_team"] = serde_json::json!("CT");
    let round_end = post(addr, "/gsi", &next.to_string());
    assert_eq!(status_of(&round_end), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::RoundEnd]);

    next["round"]["phase"] = serde_json::json!("freezetime");
    next["map"]["round"] = serde_json::json!(6);
    next["map"]["team_ct"]["score"] = serde_json::json!(4);
    next["bomb"]["state"] = serde_json::json!("carried");
    assert_eq!(status_of(&post(addr, "/gsi", &next.to_string())), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::RoundStart]);

    next["round"]["phase"] = serde_json::json!("live");
    next["player"]["match_stats"]["kills"] = serde_json::json!(13);
    assert_eq!(status_of(&post(addr, "/gsi", &next.to_string())), 200);
    let kill = harness.drained();
    assert_eq!(kinds(&kill), vec![EventKind::Kill]);
    let payload = kill[0].payload.as_deref().unwrap_or("");
    assert!(payload.contains("\"total\":13"), "got {payload}");
    assert!(payload.contains("\"round\":6"), "got {payload}");

    // 4. The match ends.
    next["map"]["phase"] = serde_json::json!("gameover");
    assert_eq!(status_of(&post(addr, "/gsi", &next.to_string())), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameEnd]);
}

#[test]
fn a_post_with_a_wrong_token_is_rejected_and_derives_nothing() {
    let harness = Harness::start();
    let addr = harness.addr();

    // A token that is one character different, one that is shorter, one that is longer, and
    // an empty one: all refused, and none of them advances the game state.
    for wrong in [
        "fixture-token-not-a-secreu",
        "fixture-token-not-a-secret ",
        "fixture-token-not-a-secretx",
        "",
    ] {
        let body = with_token(CS2_COMPETITIVE, wrong);
        let response = post(addr, "/gsi", &body);
        assert_eq!(status_of(&response), 403, "token {wrong:?} must be refused");
        assert!(
            !response.contains(TOKEN),
            "the response must not echo the configured token: {response}"
        );
    }
    assert!(harness.drained().is_empty(), "a refused payload derives nothing");
}

#[test]
fn a_post_with_no_auth_block_at_all_is_rejected() {
    let harness = Harness::start();
    let addr = harness.addr();

    let body = without_auth(CS2_COMPETITIVE);
    assert!(!body.contains("auth"), "the fixture really has no auth block: {body}");
    assert_eq!(status_of(&post(addr, "/gsi", &body)), 403);
    assert!(harness.drained().is_empty());

    // And the listener is still perfectly willing to serve a good payload afterwards: a
    // rejection is not a broken listener.
    assert_eq!(status_of(&post(addr, "/gsi", CS2_COMPETITIVE)), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameStart]);
}

#[test]
fn a_malformed_body_yields_nothing_and_does_not_stop_the_listener() {
    let harness = Harness::start();
    let addr = harness.addr();

    for body in [
        "not json at all",
        "{",
        "{\"provider\": {\"appid\": 730}, \"map\": {\"round\": \"five\"}}",
        "[]",
        "",
        "{\"auth\":{\"token\":\"fixture-token-not-a-secret\"", // truncated mid-token
    ] {
        let response = post(addr, "/gsi", body);
        assert_eq!(status_of(&response), 400, "{body:?} must be refused, got {response}");
    }
    assert!(harness.drained().is_empty(), "no payload derived anything");

    // The listener survived all of that and still works — the point of the test.
    assert_eq!(status_of(&post(addr, "/gsi", CS2_COMPETITIVE)), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameStart]);
}

#[test]
fn a_body_with_no_declared_length_is_refused_before_it_is_read() {
    let harness = Harness::start();
    let addr = harness.addr();

    let request = format!(
        "POST /gsi HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
        CS2_COMPETITIVE
    );
    assert_eq!(status_of(&raw_request(addr, request.as_bytes())), 411);
    assert!(harness.drained().is_empty());

    let chunked = format!(
        "POST /gsi HTTP/1.1\r\nHost: {addr}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n"
    );
    assert_eq!(status_of(&raw_request(addr, chunked.as_bytes())), 411, "GSI sends a length");
}

#[test]
fn an_over_long_body_is_refused_and_a_shorter_one_than_claimed_is_too() {
    let harness = Harness::start();
    let addr = harness.addr();

    let huge = format!(
        "POST /gsi HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        gsi::http::MAX_BODY_BYTES + 1
    );
    assert_eq!(status_of(&raw_request(addr, huge.as_bytes())), 413);

    let lying = format!(
        "POST /gsi HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n{{}}"
    );
    assert_eq!(status_of(&raw_request(addr, lying.as_bytes())), 413);

    assert!(harness.drained().is_empty());
}

#[test]
fn a_dota_payload_is_derived_from_its_own_scoreboard() {
    let harness = Harness::start();
    let addr = harness.addr();

    assert_eq!(status_of(&post(addr, "/gsi", DOTA_IN_GAME)), 200);
    let attach = harness.drained();
    assert_eq!(kinds(&attach), vec![EventKind::GameStart]);
    let payload = attach[0].payload.as_deref().unwrap_or("");
    assert!(payload.contains("\"game\":\"dota2\""), "got {payload}");
    assert!(payload.contains("\"adopted_scoreboard\""), "got {payload}");

    let mut next: serde_json::Value = serde_json::from_str(DOTA_IN_GAME).expect("JSON");
    next["player"]["kills"] = serde_json::json!(5);
    next["player"]["deaths"] = serde_json::json!(3);
    assert_eq!(status_of(&post(addr, "/gsi", &next.to_string())), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::Kill, EventKind::Death]);
}

#[test]
fn the_main_menu_never_produces_an_event() {
    // A client sitting in a menu posts the same shape as a match that has not started; it
    // must never be mistaken for one, and it must never end a game either.
    let harness = Harness::start();
    let addr = harness.addr();

    for _ in 0..3 {
        assert_eq!(status_of(&post(addr, "/gsi", CS2_MENU)), 200);
    }
    assert!(harness.drained().is_empty());

    // A match starts, and the menu afterwards ends it — once.
    assert_eq!(status_of(&post(addr, "/gsi", CS2_COMPETITIVE)), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameStart]);
    for _ in 0..3 {
        assert_eq!(status_of(&post(addr, "/gsi", CS2_MENU)), 200);
    }
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameEnd]);
}

#[test]
fn a_listener_started_without_a_token_refuses_to_start() {
    let (sink, _events) = channel();
    let err = gsi::spawn(GsiConfig::new(0, ""), sink).map(|_| ()).expect_err("no token");
    assert!(format!("{err:#}").contains("token"), "got: {err:#}");
    assert_eq!(with_token(CS2_COMPETITIVE, TOKEN), CS2_COMPETITIVE);
}
