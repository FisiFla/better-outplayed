//! The League poller against a local HTTPS mock server — no game, no Riot, no network.
//!
//! # What this proves
//!
//! The mock is a real TLS server on `127.0.0.1` presenting a **self-signed certificate**,
//! which is the shape of Riot's Live Client Data API. So the whole path is exercised for
//! real: the relaxed TLS handshake, the hand-written HTTP request, the response framing, the
//! payload parse, the derivation, the logging discipline and the sink.
//!
//! In particular it proves the thing that cannot be proved any other way without touching a
//! real client: that the relaxed client accepts a certificate nothing has signed, and that a
//! client *with* verification refuses the very same certificate. Both halves are asserted
//! below, so the relaxation is a demonstrable behaviour rather than a flag someone read.
//!
//! # What this does not prove
//!
//! That Riot's payloads look like the canned ones. They are shaped from the documented API;
//! no League client was contacted, by requirement. A real client is the only thing that can
//! confirm the shapes, and that check is listed as unverified in
//! `docs/plans/2026-09-23-localplay-phase-4-integrations.md`.
//!
//! Every socket here is on loopback, and the test never POSTs to anything: the mock is a
//! plain TLS responder inside this process, and the poller only ever issues `GET`s to it.

use localplay_events::lol::{Endpoint, LolConfig, LolPoller, Response};
use localplay_events::{EventKind, GameEvent};
use native_tls::{Identity, TlsAcceptor, TlsConnector};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

const CERT: &[u8] = include_bytes!("fixtures/tls/cert.pem");
const KEY: &[u8] = include_bytes!("fixtures/tls/key-pkcs8.pem");

const IN_PROGRESS: &str = include_str!("fixtures/lol_in_progress.json");
const EVENTS_NEXT: &str = include_str!("fixtures/lol_events_next.json");
const SECOND_GAME: &str = include_str!("fixtures/lol_second_game.json");
const NO_IDENTITY: &str = include_str!("fixtures/lol_no_identity.json");
const NONE: &str = include_str!("fixtures/lol_none.json");

/// A game that is present but has reported no events yet — the loading screen, and the
/// document an attach is tested against when the point is what happens *after* it.
const LOADING: &str = r#"{"gameData":{"gameTime":-25.0,"mapName":"Map11"}}"#;

/// What the mock answers with.
#[derive(Debug, Clone, Copy)]
enum Reply {
    /// `HTTP/1.1 200` and this body.
    Body(&'static str),
    /// `HTTP/1.1 404` and no body — the Live Client between games.
    NoGame,
}

/// A stand-in for the Live Client: a self-signed HTTPS server on loopback that answers each
/// request with the next queued reply. The last reply repeats, so a test can poll more times
/// than it queued answers without the server inventing anything.
struct MockGame {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<Mutex<bool>>,
    thread: Option<JoinHandle<()>>,
}

impl MockGame {
    fn start(replies: Vec<Reply>) -> Self {
        let identity = Identity::from_pkcs8(CERT, KEY).expect("the fixture certificate loads");
        let acceptor = TlsAcceptor::new(identity).expect("a TLS acceptor");
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral loopback port");
        listener.set_nonblocking(true).expect("a non-blocking listener");
        let addr = listener.local_addr().expect("the port the OS assigned");

        let queue = Arc::new(Mutex::new(VecDeque::from(replies)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(Mutex::new(false));

        let thread = {
            let queue = Arc::clone(&queue);
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("lol-mock".to_string())
                .spawn(move || {
                    while !*stop.lock().expect("the stop flag") {
                        match listener.accept() {
                            Ok((stream, _)) => serve_one(stream, &acceptor, &queue, &requests),
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => break,
                        }
                    }
                })
                .expect("the mock thread starts")
        };

        Self { addr, requests, stop, thread: Some(thread) }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::loopback(self.addr, "/liveclientdata/allgamedata").expect("a loopback endpoint")
    }

    fn poller(&self) -> LolPoller {
        LolPoller::new(LolConfig { endpoint: self.endpoint(), interval: Duration::from_millis(20) })
            .expect("a client for the mock")
    }

    /// The request lines the mock has seen, so a test can assert on the wire as well as on
    /// the derived events.
    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("the request log").clone()
    }
}

impl Drop for MockGame {
    fn drop(&mut self) {
        if let Ok(mut stop) = self.stop.lock() {
            *stop = true;
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Read one request and answer it, or do nothing at all if the client never finishes
/// sending one (the strict-verification case below: the handshake is refused and the
/// connection is dropped).
fn serve_one(
    stream: TcpStream,
    acceptor: &TlsAcceptor,
    queue: &Arc<Mutex<VecDeque<Reply>>>,
    requests: &Arc<Mutex<Vec<String>>>,
) {
    // The listener is non-blocking so the mock can be stopped; on BSD-derived platforms an
    // accepted socket inherits that, and a non-blocking handshake fails with `WouldBlock`.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut tls = match acceptor.accept(stream) {
        Ok(tls) => tls,
        Err(_) => return,
    };
    let mut buf = [0u8; 2048];
    let read = match tls.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };
    let request = String::from_utf8_lossy(&buf[..read]).to_string();
    if let Some(line) = request.lines().next() {
        requests.lock().expect("the request log").push(line.to_string());
    }

    let reply = {
        let mut queue = queue.lock().expect("the reply queue");
        // The last reply repeats: a test that polls five times against one queued answer is
        // asking "and if it says the same thing again?"
        if queue.len() > 1 {
            queue.pop_front().unwrap_or(Reply::NoGame)
        } else {
            queue.front().copied().unwrap_or(Reply::NoGame)
        }
    };
    let response = match reply {
        Reply::Body(body) => format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
        Reply::NoGame => {
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
        }
    };
    let _ = tls.write_all(response.as_bytes());
    let _ = tls.flush();
    // Give the client time to read before the socket closes under it.
    std::thread::sleep(Duration::from_millis(20));
}

fn kinds(events: &[GameEvent]) -> Vec<EventKind> {
    events.iter().map(|e| e.kind).collect()
}

fn payload_of(events: &[GameEvent], kind: EventKind) -> &str {
    events
        .iter()
        .find(|e| e.kind == kind)
        .and_then(|e| e.payload.as_deref())
        .unwrap_or_else(|| panic!("no {kind} in {events:?}"))
}

#[test]
fn the_relaxed_client_accepts_the_self_signed_certificate_and_a_strict_one_does_not() {
    // The security property the whole LoL exemption rests on, tested from both sides. If
    // this test ever passes for the wrong reason (a certificate that some authority *did*
    // sign, or a client that stopped verifying), the failure below catches it.
    let mock = MockGame::start(vec![Reply::Body(IN_PROGRESS)]);
    let addr = mock.addr;

    // 1. A stock client — full verification — must refuse this certificate.
    let strict = TlsConnector::builder().build().expect("a default connector");
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("loopback connects");
    let refused = strict.connect("127.0.0.1", tcp);
    assert!(
        refused.is_err(),
        "the fixture certificate must be untrusted, or this test proves nothing"
    );

    // 2. localplay's own client accepts it and gets the game document.
    let mut poller = mock.poller();
    let events = poller.poll_once();
    assert_eq!(kinds(&events), vec![EventKind::GameStart], "the attach, through real TLS");
    assert!(poller.in_game());
}

#[test]
fn attaching_to_a_game_already_in_progress_does_not_replay_its_history() {
    // The event storm this exists to prevent: six events are already in the document, two of
    // which are the watched player's kill and death.
    let mock = MockGame::start(vec![Reply::Body(IN_PROGRESS)]);
    let mut poller = mock.poller();

    assert_eq!(
        kinds(&poller.poll_once()),
        vec![EventKind::GameStart],
        "one event — the attach — not one per event in the history"
    );

    // And every further poll of the same document is silent: the poller is idempotent.
    for poll in 1..=5 {
        assert!(
            poller.poll_once().is_empty(),
            "poll {poll} of an unchanged game must emit nothing"
        );
    }

    // The request went to the path the spec names, over the endpoint the poller was given.
    let requests = mock.requests();
    assert!(requests.len() >= 2, "the mock saw the polls: {requests:?}");
    assert_eq!(requests[0], "GET /liveclientdata/allgamedata HTTP/1.1");
}

#[test]
fn a_full_event_history_is_derived_in_order_and_each_event_is_emitted_once() {
    // Two documents: the game with an empty history, then the game with the history. This is
    // the shape a real start has — the loading screen serves almost nothing — and it is the
    // one case where the history's events *are* new and must be reported.
    let mock = MockGame::start(vec![
        Reply::Body(LOADING),
        Reply::Body(IN_PROGRESS),
        Reply::Body(EVENTS_NEXT),
        Reply::Body(EVENTS_NEXT),
    ]);
    let mut poller = mock.poller();

    // Attach: a game, no history yet.
    assert_eq!(kinds(&poller.poll_once()), vec![EventKind::GameStart]);

    // The history arrives: the player's kill, the player's death, a dragon — and nothing for
    // the champion kill between two other players.
    let history = poller.poll_once();
    assert_eq!(
        kinds(&history),
        vec![EventKind::Kill, EventKind::Death, EventKind::Objective]
    );
    assert!(payload_of(&history, EventKind::Kill).contains("\"killer\":\"FisiFla#EUW\""));
    assert!(payload_of(&history, EventKind::Death).contains("\"victim\":\"FisiFla#EUW\""));
    assert!(payload_of(&history, EventKind::Objective).contains("\"monster\":\"DRAGON\""));

    // Two more events arrive later, one at a time.
    assert_eq!(kinds(&poller.poll_once()), vec![EventKind::Kill, EventKind::Objective]);
    assert!(poller.poll_once().is_empty(), "the same document, again");
}

#[test]
fn an_incremental_event_is_emitted_once_and_the_same_poll_twice_is_a_no_op() {
    let mock = MockGame::start(vec![Reply::Body(IN_PROGRESS), Reply::Body(EVENTS_NEXT)]);
    let mut poller = mock.poller();
    poller.poll_once(); // attach

    let incremental = poller.poll_once();
    assert_eq!(kinds(&incremental), vec![EventKind::Kill, EventKind::Objective]);
    assert!(payload_of(&incremental, EventKind::Objective).contains("\"stolen\":true"));
    assert!(poller.poll_once().is_empty(), "the mock repeats its last reply");
}

#[test]
fn the_game_ending_emits_one_end_event_and_every_later_poll_is_silent() {
    // 404s at the end: the Live Client's answer once the game is over.
    let mock = MockGame::start(vec![Reply::Body(EVENTS_NEXT), Reply::NoGame]);
    let mut poller = mock.poller();

    assert_eq!(kinds(&poller.poll_once()), vec![EventKind::GameStart]);
    let ended = poller.poll_once();
    assert_eq!(kinds(&ended), vec![EventKind::GameEnd]);
    assert!(!poller.in_game());
    assert!(
        payload_of(&ended, EventKind::GameEnd).contains("last_game_time_s"),
        "the record says how long the game ran"
    );
    for poll in 1..=3 {
        assert!(poller.poll_once().is_empty(), "poll {poll} after the game ended");
    }
}

#[test]
fn a_connection_that_is_refused_is_the_no_game_case() {
    // Nothing listening at all — the ordinary state of a machine with no League client
    // running. The poll must be empty rather than an error, and it must not invent a
    // GameEnd for a game that was never seen.
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let addr = listener.local_addr().unwrap();
    drop(listener); // the port is now closed

    let endpoint = Endpoint::loopback(addr, "/liveclientdata/allgamedata").unwrap();
    let mut poller = LolPoller::new(LolConfig {
        endpoint,
        interval: Duration::from_millis(10),
    })
    .expect("a client for a closed port");

    for _ in 0..3 {
        assert!(poller.poll_once().is_empty(), "a refused connection is not an event");
    }
    assert!(!poller.in_game());
}

#[test]
fn a_malformed_body_is_neither_an_event_nor_the_end_of_the_game() {
    // 1. A game is being played.
    // 2. The endpoint then answers something that is not JSON.
    // 3. It recovers, with one new event.
    //
    // The malformed answer must not end the game (the state is kept) and must not be
    // reported as a stale event later: step 3 emits exactly the one new line.
    let broken = r#"{"gameData": {"gameTime": "soon"}}"#;
    let mock = MockGame::start(vec![
        Reply::Body(LOADING),
        Reply::Body(IN_PROGRESS),
        Reply::Body(broken),
        Reply::Body(broken),
        Reply::Body(EVENTS_NEXT),
    ]);
    let mut poller = mock.poller();

    assert_eq!(kinds(&poller.poll_once()), vec![EventKind::GameStart]);
    assert_eq!(
        kinds(&poller.poll_once()),
        vec![EventKind::Kill, EventKind::Death, EventKind::Objective]
    );
    assert!(poller.poll_once().is_empty(), "a body localplay cannot read is not an event");
    assert!(poller.poll_once().is_empty(), "and it is not reported again");
    assert!(poller.in_game(), "one unreadable response is not the end of a game");

    // Recovery: only the two new events, nothing replayed.
    assert_eq!(kinds(&poller.poll_once()), vec![EventKind::Kill, EventKind::Objective]);
    assert!(poller.poll_once().is_empty());
}

#[test]
fn a_null_body_is_treated_as_the_game_being_over() {
    // `null` is a documented "no game" answer, distinct from an empty object: the game it
    // was watching has ended.
    let mock = MockGame::start(vec![Reply::Body(IN_PROGRESS), Reply::Body(NONE)]);
    let mut poller = mock.poller();
    assert_eq!(kinds(&poller.poll_once()), vec![EventKind::GameStart]);
    assert_eq!(kinds(&poller.poll_once()), vec![EventKind::GameEnd]);
    assert!(!poller.in_game());
    assert!(poller.poll_once().is_empty());
}

#[test]
fn a_second_game_that_starts_without_the_endpoint_going_quiet_is_a_new_attach() {
    // The player quit and requeued: the clock restarts, the ids restart at 0, and the
    // endpoint never 404s. The second game must be reported as one attach, not as a replay
    // of its early history.
    let mock = MockGame::start(vec![
        Reply::Body(IN_PROGRESS),
        Reply::Body(SECOND_GAME),
        Reply::Body(SECOND_GAME),
    ]);
    let mut poller = mock.poller();

    assert_eq!(kinds(&poller.poll_once()), vec![EventKind::GameStart]);
    let restart = poller.poll_once();
    assert_eq!(kinds(&restart), vec![EventKind::GameStart], "one attach, not two events");
    assert!(
        payload_of(&restart, EventKind::GameStart).contains("\"adopted_in_progress_events\":2"),
        "the new game's history is adopted: {}",
        payload_of(&restart, EventKind::GameStart)
    );
    assert!(poller.poll_once().is_empty());
}

#[test]
fn a_document_that_names_no_player_still_reports_a_kill_it_sees() {
    let mock = MockGame::start(vec![
        Reply::Body(LOADING),
        Reply::Body(NO_IDENTITY),
    ]);
    let mut poller = mock.poller();
    poller.poll_once(); // attach on an empty history
    let events = poller.poll_once();
    assert_eq!(kinds(&events), vec![EventKind::Kill]);
    assert!(payload_of(&events, EventKind::Kill).contains("\"killer\":\"Somebody\""));
}

#[test]
fn the_poller_only_ever_asks_for_loopback() {
    // The one constructor that accepts an address refuses anything else, so a configuration
    // mistake cannot turn the poller into an outbound HTTP client.
    let non_loopback: SocketAddr = "192.168.1.10:2999".parse().unwrap();
    let err = Endpoint::loopback(non_loopback, "/liveclientdata/allgamedata")
        .expect_err("not loopback");
    assert!(err.to_string().contains("loopback"), "got: {err}");

    // The endpoint a poller was built with is the one it dialled, and it is loopback.
    let mock = MockGame::start(vec![Reply::NoGame]);
    let poller = mock.poller();
    assert!(poller.endpoint().is_loopback());
    assert_eq!(poller.endpoint().addr(), mock.addr);

    // And a response type is what the client reports for a 404 rather than an error.
    let client = localplay_events::lol::LoopbackClient::for_endpoint(mock.endpoint())
        .expect("a relaxed client for the mock");
    assert_eq!(
        client.get().expect("the mock answers"),
        Response::NotServing,
        "a 404 is a value, not an error: it is how the endpoint ends a game"
    );
}
