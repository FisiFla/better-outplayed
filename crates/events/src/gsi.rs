//! CS2 / Dota 2 — Valve Game State Integration (spec §7.2).
//!
//! A listener on `127.0.0.1` that consumes the JSON the game POSTs, derives events and
//! hands them to a driver. The user installs the generated
//! `gamestate_integration_localplay.cfg` into the game's `cfg` directory; the application
//! never writes there itself ([`integration`] produces the text and nothing else).
//!
//! # The three constraints the spec puts on this listener
//!
//! 1. **Loopback only, by construction.** [`bind`] refuses any address that is not
//!    loopback, and the only other way in is [`bind_loopback`], which takes a port and
//!    nothing else. There is no configuration setting for a bind address, so "discouraged"
//!    never enters into it: a listener on `0.0.0.0` cannot be expressed.
//! 2. **A token, checked in constant time.** The token travels in the payload's `auth`
//!    block (that is how GSI's `"auth"` section works; there is no header), and a request
//!    whose token is missing or different is refused and dropped — see [`token_matches`].
//!    The listener answers `403` and reads nothing further.
//! 3. **No attacker-controlled content in the log.** A refused or unreadable request is
//!    *counted*, and the log line says how many and what kind. The request's body, its
//!    headers and its token are never written anywhere.
//!
//! # What it does not do
//!
//! It does not distinguish a Dota 2 client from a CS2 client by anything other than the
//! payload (the app id, then the provider name — see [`derive::Game`]). It does not react
//! to the game exiting: if the process is killed there is no final POST and no event, so a
//! `GameEnd` is only derived from a payload that says so (`map.phase == "gameover"`, Dota's
//! `POST_GAME`). That gap is real, is recorded in `docs/plans/2026-09-23-localplay-phase-4-integrations.md`, and
//! is why the League integration's end-of-game signal (the endpoint going quiet) is
//! different from this one.

pub mod derive;
pub mod http;
pub mod integration;
pub mod payload;

pub use derive::{Game, Tracker};
pub use integration::{
    generate_token, integration_cfg, integration_cfg_dota, load_or_create_token, DEFAULT_PATH,
    TOKEN_BYTES,
};
pub use payload::GsiSnapshot;

use crate::wire::write_response;
use crate::{EventSink, GameEvent};
use anyhow::{bail, Context, Result};
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use subtle::ConstantTimeEq;

/// How long one request may take to arrive and be read. A game POSTs a small body to
/// loopback; two seconds is generous, and it is what bounds a connection that stalls.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// How often the accept loop looks at its stop flag. The listener is idle almost all the
/// time, so this is a sleep, not a spin.
const ACCEPT_SLICE: Duration = Duration::from_millis(25);

/// Everything the listener needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GsiConfig {
    /// The port from `[events] gsi_port` (spec §10).
    pub port: u16,
    /// The token from the user's `gamestate_integration_localplay.cfg`. Never logged.
    pub token: String,
    /// The path the game is told to POST to; anything else is a 404.
    pub path: String,
}

impl GsiConfig {
    /// The production configuration: the default path, and the caller's token.
    pub fn new(port: u16, token: impl Into<String>) -> Self {
        Self { port, token: token.into(), path: DEFAULT_PATH.to_string() }
    }
}

/// Bind a listener, refusing any address that is not loopback.
///
/// The check is the point of this function existing rather than a bare `TcpListener::bind`:
/// `0.0.0.0` is the mistake worth preventing, because it is the one that silently exposes
/// the game's state to the network.
pub fn bind(addr: SocketAddr) -> Result<TcpListener> {
    if !addr.ip().is_loopback() {
        bail!("the GSI listener is a loopback service and refuses to bind {addr}");
    }
    TcpListener::bind(addr).with_context(|| format!("binding the GSI listener to {addr}"))
}

/// Bind the GSI listener on `127.0.0.1:<port>`. The only constructor production uses.
pub fn bind_loopback(port: u16) -> Result<TcpListener> {
    bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
}

/// Whether `snapshot` carries the expected token (spec §7.2).
///
/// Fails closed in three ways: no token in the payload, a token of a different length, and
/// a token of the right length that differs by one byte are all `false`. The comparison is
/// constant time ([`subtle`]) so that a local process cannot learn the token one byte at a
/// time by measuring how long a rejection takes. An empty *expected* token is a
/// programming error and is also refused: a listener configured with no token would
/// otherwise accept every payload that declares an empty one.
pub fn token_matches(expected: &str, snapshot: &GsiSnapshot) -> bool {
    if expected.is_empty() {
        return false;
    }
    let Some(presented) = snapshot.auth.get("token") else {
        return false;
    };
    presented.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// A running listener.
pub struct GsiListener {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    addr: SocketAddr,
}

impl GsiListener {
    /// The address actually bound. Inside a test this is an ephemeral port; the property
    /// that matters is that its IP is loopback.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop accepting and wait for the thread. Idempotent.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for GsiListener {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Start the listener. Every derived event goes to `sink` until the handle is dropped.
pub fn spawn(config: GsiConfig, sink: EventSink) -> Result<GsiListener> {
    if config.token.is_empty() {
        // Refused at startup rather than failing every request later: a listener that
        // cannot authenticate anything is a listener that should not be running.
        bail!("the GSI listener needs a token; generate one and put it in the game's cfg file");
    }
    let listener = bind_loopback(config.port)?;
    let addr = listener.local_addr().context("reading the listener's address")?;
    listener.set_nonblocking(true).context("making the listener non-blocking")?;

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    tracing::info!(
        "the GSI listener is up on http://{addr}{} (loopback only)",
        config.path
    );

    let thread = std::thread::Builder::new()
        .name("localplay-gsi".to_string())
        .spawn(move || serve(listener, &config, &sink, &flag))
        .context("spawning the GSI thread")?;

    Ok(GsiListener { stop, thread: Some(thread), addr })
}

/// The accept loop. One connection at a time: GSI traffic is a handful of POSTs a second
/// from a single client, and a connection is bounded by [`REQUEST_TIMEOUT`], so a queue of
/// worker threads would cost more than it buys.
fn serve(listener: TcpListener, config: &GsiConfig, sink: &EventSink, stop: &AtomicBool) {
    let mut tracker = Tracker::default();
    let mut counters = Counters::default();

    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _peer)) => {
                // `_peer` is not logged: on a loopback bind it is `127.0.0.1` by
                // definition, and it is not what a reader of the log needs.
                if handle(&mut stream, config, &mut tracker, sink, &mut counters).is_err() {
                    tracing::debug!("the GSI listener stopped: nothing is receiving events");
                    return;
                }
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => std::thread::sleep(ACCEPT_SLICE),
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => {
                tracing::warn!("the GSI listener could not accept a connection: {err}");
                std::thread::sleep(ACCEPT_SLICE);
            }
        }
    }
    tracing::debug!("the GSI listener stopped");
}

/// Serve one connection. Returns `Err` when the sink is gone, which is how the listener
/// learns that the application is shutting down.
fn handle(
    stream: &mut TcpStream,
    config: &GsiConfig,
    tracker: &mut Tracker,
    sink: &EventSink,
    counters: &mut Counters,
) -> Result<()> {
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_nodelay(true);

    let request = match http::read_request(stream, &config.path) {
        Ok(request) => request,
        Err(reject) => {
            counters.refused(reject);
            let _ = write_response(stream, reject.status());
            return Ok(());
        }
    };

    let snapshot = match payload::parse(&request.body) {
        Ok(snapshot) => snapshot,
        Err(_) => {
            // Deliberately silent about *what* was wrong: the body is attacker-controlled
            // at this point (the token gate has not run), and `serde_json`'s message can
            // quote it. The size and the count are enough to debug an installation.
            counters.malformed(request.body.len());
            let _ = write_response(stream, 400);
            return Ok(());
        }
    };

    if !token_matches(&config.token, &snapshot) {
        counters.refused_token();
        let _ = write_response(stream, 403);
        return Ok(());
    }

    let events = tracker.observe(&snapshot);
    counters.accepted();
    for event in events {
        log_event(&event);
        // A disconnected sink means the driver is gone; there is nothing to listen for.
        if sink.send(event).is_err() {
            return Err(anyhow::anyhow!("the GSI event sink is disconnected"));
        }
    }
    // Answer last: a client that reads the response knows the payload was taken.
    let _ = write_response(stream, 200);
    Ok(())
}

/// One line per derived event, at the level it deserves: the transitions of a game are
/// `info`, each kill is `debug`. The payload is written by [`derive`], never echoed from
/// the request, so quoting it here is safe.
fn log_event(event: &GameEvent) {
    match event.kind {
        crate::EventKind::GameStart => {
            tracing::info!("a GSI game is being watched: {}", event.payload.as_deref().unwrap_or(""))
        }
        crate::EventKind::GameEnd => tracing::info!("the GSI game is over"),
        kind => tracing::debug!("GSI event: {kind} {}", event.payload.as_deref().unwrap_or("")),
    }
}

/// How many requests of each kind the listener has seen, so a condition is stated once and
/// then counted. Without this, anything scanning loopback ports could fill the log.
#[derive(Debug, Default)]
struct Counters {
    accepted: u64,
    refused: u64,
    refused_tokens: u64,
    malformed: u64,
}

impl Counters {
    fn accepted(&mut self) {
        self.accepted += 1;
    }

    /// A request that was refused before it was read (wrong method, wrong path, no length,
    /// too large, not HTTP).
    fn refused(&mut self, reject: http::Reject) {
        self.refused += 1;
        if self.refused == 1 {
            tracing::info!(
                "the GSI listener refused a request: {reject}. This is normal for anything \
                 other than the game's own POST; localplay answers it with HTTP {}.",
                reject.status()
            );
        } else {
            tracing::debug!("the GSI listener has refused {} requests", self.refused);
        }
    }

    /// A well-formed payload whose token did not match: the case a user hits when the
    /// generated cfg file was not re-installed after the token changed.
    fn refused_token(&mut self) {
        self.refused_tokens += 1;
        if self.refused_tokens == 1 {
            tracing::warn!(
                "the GSI listener rejected a payload whose token does not match. Install the \
                 generated gamestate_integration_localplay.cfg into the game's cfg directory \
                 (it carries the token) and restart the game."
            );
        } else {
            tracing::debug!("the GSI listener has rejected {} payloads by token", self.refused_tokens);
        }
    }

    fn malformed(&mut self, bytes: usize) {
        self.malformed += 1;
        if self.malformed == 1 {
            tracing::warn!(
                "the GSI listener could not parse a {bytes}-byte payload; the body was \
                 dropped and the request answered with HTTP 400"
            );
        } else {
            tracing::debug!("the GSI listener has dropped {} unparseable payloads", self.malformed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn snapshot(body: &str) -> GsiSnapshot {
        payload::parse(body.as_bytes()).expect("the fixture parses")
    }

    #[test]
    fn a_listener_refuses_to_bind_anything_that_is_not_loopback() {
        for addr in ["0.0.0.0:0", "192.168.0.1:45671", "[::]:45671", "8.8.8.8:80"] {
            let addr: SocketAddr = addr.parse().expect("a socket address");
            let err = bind(addr).map(|_| ()).expect_err("only loopback may be bound");
            assert!(
                err.to_string().contains("loopback"),
                "the error must name the rule: {err}"
            );
        }
    }

    #[test]
    fn the_listener_binds_loopback_and_an_ephemeral_port() {
        // Port 0: an ephemeral port, which is what keeps a test suite from fighting over a
        // fixed one. The address is still loopback.
        let listener = bind_loopback(0).expect("a loopback listener");
        let addr = listener.local_addr().unwrap();
        assert!(addr.ip().is_loopback(), "bound to {addr}");
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(addr.port(), 0, "the OS assigned a real port");
    }

    #[test]
    fn the_token_gate_accepts_only_the_exact_token() {
        let expected = "0123456789abcdef0123456789abcdef";
        let with = |token: &str| {
            snapshot(&format!(r#"{{"auth":{{"token":"{token}"}},"provider":{{"appid":730}}}}"#))
        };

        assert!(token_matches(expected, &with(expected)), "the same token is accepted");
        assert!(!token_matches(expected, &with("0123456789abcdef0123456789abcdee")), "one byte off");
        assert!(!token_matches(expected, &with("0123456789abcdef0123456789abcde")), "one byte short");
        assert!(!token_matches(expected, &with("0123456789abcdef0123456789abcdefg")), "one byte long");
        assert!(!token_matches(expected, &with("")), "an empty token is not a token");
        assert!(
            !token_matches(expected, &snapshot(r#"{"provider":{"appid":730}}"#)),
            "a payload with no auth block is refused"
        );
        assert!(
            !token_matches(expected, &snapshot(r#"{"auth":{"kind":"cs2"}}"#)),
            "an auth block without a token is refused"
        );
        assert!(
            !token_matches("", &with("")),
            "an unconfigured token fails closed rather than accepting an empty one"
        );
    }

    #[test]
    fn the_token_is_compared_over_the_whole_value() {
        // A timing test would be flaky; what is asserted instead is the property the
        // constant-time comparison exists for — that no prefix of a token is ever accepted,
        // at any length, so there is no early-accept path to time.
        let expected = "abcdef0123456789abcdef0123456789";
        for end in 0..expected.len() {
            let prefix = &expected[..end];
            assert!(
                !token_matches(expected, &snapshot(&format!(
                    r#"{{"auth":{{"token":"{prefix}"}}}}"#
                ))),
                "a {end}-byte prefix must not be accepted"
            );
        }
    }

    #[test]
    fn a_listener_without_a_token_refuses_to_start() {
        let (sink, _rx) = std::sync::mpsc::channel();
        let err = spawn(GsiConfig::new(0, ""), sink).map(|_| ()).expect_err("no token");
        assert!(format!("{err:#}").contains("token"), "got: {err:#}");
    }
}
