//! The GSI listener's per-game routing: one path and one token per game.
//!
//! The listener serves two games, each on its own path and with its own token, and each
//! binding may be pinned to Valve's app id. The binding is the mechanism that scopes a
//! token, and this is the file that proves it: a token lifted from one game's installed
//! `.cfg` is refused on the other's path, and a payload that names another app id is
//! refused even on the path the token was lifted for.
//!
//! The listener is real (bound socket, request gate, token comparison, derivation) and
//! driven by a hand-written HTTP client, like `gsi_listener.rs`. Every socket is on
//! `127.0.0.1` and every payload is a canned fixture.
//!
//! Asserted:
//! * a game posts to its own path with its own token and is served;
//! * a path no game serves is a 404, whatever token the body carries;
//! * a token that is not the path's own is a 403;
//! * a payload that names another app id is a 403, even with the right token;
//! * one game's token does not work on the other game's path;
//! * the listener reports the games it serves, and never a token.

// An integration test is a crate of its own, so the crate root's `cfg_attr(test, ...)`
// exemption does not reach it: the same rule, stated here.
#![allow(clippy::unwrap_used)]

use localplay_events::gsi::{self, GsiConfig, GsiListener};
use localplay_events::{EventKind, GameEvent};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

const CS2_COMPETITIVE: &str = include_str!("fixtures/gsi_cs2_competitive.json");
const DOTA_IN_GAME: &str = include_str!("fixtures/gsi_dota_in_game.json");

/// The token the fixtures carry, replaced in every body below. Not a secret.
const FIXTURE_TOKEN: &str = "fixture-token-not-a-secret";

/// Counter-Strike's own token, on its own path. Not a secret.
const CS2_TOKEN: &str = "cs2-fixture-token-not-a-secret";

/// Dota 2's own token, on its own path. Not a secret.
const DOTA_TOKEN: &str = "dota-fixture-token-not-a-secret";

/// The path Counter-Strike posts to.
const CS2_PATH: &str = "/gsi/cs2";

/// The path Dota 2 posts to.
const DOTA_PATH: &str = "/gsi/dota2";

/// A listener plus the channel its events arrive on.
struct Harness {
    listener: GsiListener,
    events: Receiver<GameEvent>,
}

impl Harness {
    /// A listener serving both games: each on its own path, with its own token, and
    /// pinned to its own app id.
    fn two_games() -> Self {
        let config = GsiConfig::new(0, CS2_TOKEN)
            .with_game(CS2_PATH, CS2_TOKEN, Some(gsi::CS2_APP_ID))
            .add_game(DOTA_PATH, DOTA_TOKEN, Some(gsi::DOTA2_APP_ID));
        Self::start_with(config)
    }

    /// A listener serving one game, on the path and with the token given.
    fn one_game(path: &str, token: &str, app_id: Option<u32>) -> Self {
        Self::start_with(GsiConfig::new(0, token).with_game(path, token, app_id))
    }

    fn start_with(config: GsiConfig) -> Self {
        let (sink, events) = channel();
        let listener = gsi::spawn(config, sink).expect("the listener starts on a free port");
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
    let mut stream =
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("loopback connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("a read timeout");
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
    fixture.replace(FIXTURE_TOKEN, token)
}

#[test]
fn a_game_posts_to_its_own_path_with_its_own_token() {
    let harness = Harness::two_games();
    let addr = harness.addr();

    let response = post(addr, CS2_PATH, &with_token(CS2_COMPETITIVE, CS2_TOKEN));
    assert_eq!(
        status_of(&response),
        200,
        "the path is the game's own and the token is too: {response}"
    );
    let events = harness.drained();
    assert_eq!(kinds(&events), vec![EventKind::GameStart]);
    assert!(
        events[0]
            .payload
            .as_deref()
            .unwrap_or("")
            .contains("\"map\":\"de_dust2\""),
        "the attach records which map: {:?}",
        events[0].payload
    );

    // Dota 2, on its own path, with its own token.
    let response = post(addr, DOTA_PATH, &with_token(DOTA_IN_GAME, DOTA_TOKEN));
    assert_eq!(status_of(&response), 200, "{response}");
    let events = harness.drained();
    assert_eq!(kinds(&events), vec![EventKind::GameStart]);
    assert!(
        events[0]
            .payload
            .as_deref()
            .unwrap_or("")
            .contains("\"game\":\"dota2\""),
        "the attach records which game: {:?}",
        events[0].payload
    );
}

#[test]
fn a_path_no_game_serves_is_a_404_whatever_token_the_body_carries() {
    let harness = Harness::two_games();
    let addr = harness.addr();

    // The right token, on a path no game posts to.
    for path in ["/gsi", "/", "/gsi/cs", "/gsi/cs2/", "/gsi/dota2x"] {
        let response = post(addr, path, &with_token(CS2_COMPETITIVE, CS2_TOKEN));
        assert_eq!(
            status_of(&response),
            404,
            "{path} must not be served: {response}"
        );
    }
    assert!(
        harness.drained().is_empty(),
        "a request on a path no game serves derives nothing"
    );

    // The listener is unharmed: the game still gets through.
    let response = post(addr, CS2_PATH, &with_token(CS2_COMPETITIVE, CS2_TOKEN));
    assert_eq!(status_of(&response), 200, "{response}");
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameStart]);
}

#[test]
fn a_token_that_is_not_the_paths_own_is_a_403() {
    let harness = Harness::two_games();
    let addr = harness.addr();

    // Dota 2's token on Counter-Strike's path, and the other way round.
    for (path, body, token) in [
        (CS2_PATH, CS2_COMPETITIVE, DOTA_TOKEN),
        (DOTA_PATH, DOTA_IN_GAME, CS2_TOKEN),
    ] {
        let response = post(addr, path, &with_token(body, token));
        assert_eq!(
            status_of(&response),
            403,
            "{path} does not take {token}: {response}"
        );
    }
    assert!(
        harness.drained().is_empty(),
        "a refused payload derives nothing"
    );
}

#[test]
fn one_games_token_does_not_work_on_the_other_games_path() {
    // The point of the binding: the token is scoped to the path it was installed for, so a
    // token lifted from Counter-Strike's `.cfg` cannot forge Dota 2's events.
    let harness = Harness::two_games();
    let addr = harness.addr();

    let response = post(addr, DOTA_PATH, &with_token(DOTA_IN_GAME, CS2_TOKEN));
    assert_eq!(
        status_of(&response),
        403,
        "Counter-Strike's token must not be accepted on Dota 2's path: {response}"
    );
    assert!(
        harness.drained().is_empty(),
        "and it derives nothing: the scoreboard in it is not the watched player's"
    );

    // The other way round, for the same reason.
    let response = post(addr, CS2_PATH, &with_token(CS2_COMPETITIVE, DOTA_TOKEN));
    assert_eq!(status_of(&response), 403, "{response}");
    assert!(harness.drained().is_empty());
}

#[test]
fn a_payload_carrying_another_games_app_id_is_a_403() {
    // The path is right and the token is right, and the payload says it is another game:
    // the app id the binding is pinned to is the last gate.
    let harness = Harness::two_games();
    let addr = harness.addr();

    let response = post(addr, CS2_PATH, &with_token(DOTA_IN_GAME, CS2_TOKEN));
    assert_eq!(
        status_of(&response),
        403,
        "a Dota payload is not what the Counter-Strike binding serves: {response}"
    );
    assert!(harness.drained().is_empty(), "and it derives nothing");

    // A binding that is not pinned to an app id takes what it is given.
    let harness = Harness::one_game(CS2_PATH, CS2_TOKEN, None);
    let addr = harness.addr();
    let response = post(addr, CS2_PATH, &with_token(DOTA_IN_GAME, CS2_TOKEN));
    assert_eq!(status_of(&response), 200, "{response}");
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameStart]);
}

#[test]
fn a_query_string_does_not_hide_the_path() {
    // The path is what routes, and nothing in a query is ever read.
    let harness = Harness::two_games();
    let addr = harness.addr();

    let response = post(
        addr,
        &format!("{CS2_PATH}?token={CS2_TOKEN}"),
        &with_token(CS2_COMPETITIVE, CS2_TOKEN),
    );
    assert_eq!(status_of(&response), 200, "{response}");
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameStart]);
}

#[test]
fn the_listener_reports_the_games_it_serves_and_never_a_token() {
    let harness = Harness::two_games();
    let games = harness.listener.games();

    assert_eq!(games.len(), 2, "the games it serves: {games:?}");
    assert_eq!(games[0].path, CS2_PATH, "the first game's path");
    assert_eq!(games[0].app_id, Some(gsi::CS2_APP_ID), "and its app id");
    assert_eq!(games[1].path, DOTA_PATH, "the second game's path");
    assert_eq!(games[1].app_id, Some(gsi::DOTA2_APP_ID), "and its app id");

    // No token, in the report or in anything the report can be printed through.
    let report = format!("{games:?}");
    assert!(
        !report.contains(CS2_TOKEN),
        "the report names no token: {report}"
    );
    assert!(
        !report.contains(DOTA_TOKEN),
        "the report names no token: {report}"
    );
    assert!(
        report.contains(CS2_PATH),
        "but it does name the paths: {report}"
    );

    // The listener still answers for the token of either game, and for nothing else.
    assert!(harness.listener.accepts_token(CS2_TOKEN));
    assert!(harness.listener.accepts_token(DOTA_TOKEN));
    assert!(!harness.listener.accepts_token(""));
    assert!(!harness
        .listener
        .accepts_token("cs2-fixture-token-not-a-secreu"));
}
