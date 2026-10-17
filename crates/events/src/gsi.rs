//! CS2 / Dota 2: Valve Game State Integration.
//!
//! A listener on `127.0.0.1` that consumes the JSON the game POSTs, derives events and hands
//! them to a driver. The user installs the generated `gamestate_integration_localplay.cfg`
//! into the game's `cfg` directory; the application never writes there ([`integration`]
//! produces the text only).
//!
//! Three constraints:
//!
//! 1. Loopback only, by construction. [`bind`] refuses any non-loopback address and the only
//!    other way in, [`bind_loopback`], takes a port alone. No setting names a bind address, so
//!    a listener on `0.0.0.0` cannot be expressed.
//! 2. A token, checked in constant time. It travels in the payload's `auth` block (GSI has no
//!    header), and a request with a missing or different token is refused with `403` and
//!    nothing further is read ([`token_matches`]).
//! 3. No attacker-controlled content in the log. A refused or unreadable request is counted,
//!    and the log line says how many and what kind; the body, headers and token are never
//!    written.
//!
//! The game is told apart only by payload (app id, then provider name; see [`derive::Game`]).
//! The listener cannot react to the game exiting: a killed process sends no final POST, so
//! `GameEnd` is only derived from a payload that says so (`map.phase == "gameover"`, Dota's
//! `POST_GAME`). That is why League's end-of-game signal (the endpoint going quiet) differs.

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
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

/// How long one request may take to arrive and be read. A game POSTs a small body to loopback,
/// so 2 s is generous and bounds a stalled connection.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// The most connections served in one [`RATE_WINDOW`]. A game POSTs a handful of times a
/// second (CS2 throttles to 0.1 s at busiest); anything past this is not the game and is
/// answered `429` unread, so a local process hammering the port cannot keep the
/// single-threaded accept loop busy or flood the log.
pub const MAX_REQUESTS_PER_WINDOW: u32 = 100;

/// The length of the fixed window [`MAX_REQUESTS_PER_WINDOW`] is counted in.
pub const RATE_WINDOW: Duration = Duration::from_secs(1);

/// The most wall-clock time one request may take in total. [`REQUEST_TIMEOUT`] bounds each
/// read, which a peer trickling one byte at a time resets forever; this bounds the sum.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(5);

/// How often the accept loop checks its stop flag: a sleep, not a spin.
const ACCEPT_SLICE: Duration = Duration::from_millis(25);

/// A fixed-window request counter: [`MAX_REQUESTS_PER_WINDOW`] per [`RATE_WINDOW`].
#[derive(Debug)]
struct RateLimiter {
    window_start: Instant,
    count: u32,
}

impl RateLimiter {
    fn new(now: Instant) -> Self {
        Self { window_start: now, count: 0 }
    }

    /// Whether a connection arriving at `now` may be served.
    fn allow(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.window_start) >= RATE_WINDOW {
            self.window_start = now;
            self.count = 0;
        }
        if self.count >= MAX_REQUESTS_PER_WINDOW {
            return false;
        }
        self.count += 1;
        true
    }
}

/// A reader that fails once [`REQUEST_DEADLINE`] has passed in total, re-arming the socket's
/// read timeout with the time left before every read.
struct DeadlineReader<'a> {
    stream: &'a mut TcpStream,
    deadline: Instant,
}

impl std::io::Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(left.min(REQUEST_TIMEOUT)))?;
        self.stream.read(buf)
    }
}

/// Everything the listener needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GsiConfig {
    /// The port from `[events] gsi_port`.
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

/// Bind a listener, refusing any address that is not loopback. This check is why the function
/// exists instead of a bare `TcpListener::bind`: `0.0.0.0` would silently expose the game's
/// state to the network.
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

/// Whether `snapshot` carries the expected token.
///
/// Fails closed: no token, a token of a different length, and a right-length token differing
/// by one byte are all `false`. The comparison is constant time ([`subtle`]) so a local
/// process cannot learn the token byte by byte from rejection timing. An empty expected token
/// is a programming error and is also refused, since it would accept every payload declaring
/// an empty one.
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
    /// The address actually bound (an ephemeral port in tests); what matters is that its IP is
    /// loopback.
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
        // Refused at startup rather than failing every request later: a listener that cannot
        // authenticate anything should not be running.
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

/// The accept loop. One connection at a time: GSI is a handful of POSTs a second from one
/// client and each connection is bounded by [`REQUEST_TIMEOUT`], so worker threads would cost
/// more than they buy.
fn serve(listener: TcpListener, config: &GsiConfig, sink: &EventSink, stop: &AtomicBool) {
    let mut tracker = Tracker::default();
    let mut counters = Counters::default();
    let mut limiter = RateLimiter::new(Instant::now());

    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _peer)) => {
                // `_peer` is not logged: on a loopback bind it is `127.0.0.1` by definition.
                if !limiter.allow(Instant::now()) {
                    counters.rate_limited();
                    turn_away(&mut stream);
                    continue;
                }
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

/// Answer `429` without reading the request, then swallow a bounded amount of what the peer
/// already sent: closing a socket holding unread data resets the connection, and the reset can
/// destroy the response before the peer reads it.
fn turn_away(stream: &mut TcpStream) {
    use std::io::Read;
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    let _ = write_response(stream, 429);
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));
    let mut sink = [0u8; 4096];
    for _ in 0..4 {
        if !matches!(stream.read(&mut sink), Ok(n) if n == sink.len()) {
            break;
        }
    }
}

/// Serve one connection. `Err` means the sink is gone, i.e. the application is shutting down.
fn handle(
    stream: &mut TcpStream,
    config: &GsiConfig,
    tracker: &mut Tracker,
    sink: &EventSink,
    counters: &mut Counters,
) -> Result<()> {
    // `accept` inherits the listener's non-blocking flag on macOS/*BSD (not Linux or Windows), and
    // a non-blocking socket ignores the read/write timeouts set below: every read with no buffered
    // data fails with `WouldBlock`, which `http` reads as a malformed request (400) instead of the
    // 403/200 the client should see. Put the accepted socket back into blocking mode first so the
    // timeouts are the connection's real bound. A no-op on Windows and Linux.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_nodelay(true);

    let mut reader = DeadlineReader { stream: &mut *stream, deadline: Instant::now() + REQUEST_DEADLINE };
    let request = match http::read_request(&mut reader, &config.path) {
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
            // Deliberately silent about what was wrong: the body is attacker-controlled before the token
            // gate runs and `serde_json`'s message can quote it. The size and count suffice to debug an
            // install.
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
        // A disconnected sink means the driver is gone.
        if sink.send(event).is_err() {
            return Err(anyhow::anyhow!("the GSI event sink is disconnected"));
        }
    }
    // Answer last: a client that reads the response knows the payload was taken.
    let _ = write_response(stream, 200);
    Ok(())
}

/// One line per derived event at the level it deserves: game transitions `info`, each kill
/// `debug`. The payload is written by [`derive`], never echoed from the request, so quoting it
/// is safe.
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
/// then counted; otherwise anything scanning loopback ports could fill the log.
#[derive(Debug, Default)]
struct Counters {
    accepted: u64,
    refused: u64,
    refused_tokens: u64,
    malformed: u64,
    rate_limited: u64,
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

    /// A well-formed payload whose token did not match: what a user hits when the generated cfg
    /// was not re-installed after the token changed.
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

    fn rate_limited(&mut self) {
        self.rate_limited += 1;
        if self.rate_limited == 1 {
            tracing::warn!(
                "the GSI listener is over {MAX_REQUESTS_PER_WINDOW} connections per second and \
                 is answering the excess with HTTP 429"
            );
        } else {
            tracing::debug!("the GSI listener has rate-limited {} connections", self.rate_limited);
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

    #[test]
    fn the_rate_limiter_allows_a_window_s_worth_then_refuses_until_the_next_window() {
        let t0 = Instant::now();
        let mut limiter = RateLimiter::new(t0);
        for n in 0..MAX_REQUESTS_PER_WINDOW {
            assert!(limiter.allow(t0), "request {n} is inside the cap");
        }
        assert!(!limiter.allow(t0), "the next one is over it");
        assert!(!limiter.allow(t0 + RATE_WINDOW / 2), "and so is one later in the same window");
        assert!(limiter.allow(t0 + RATE_WINDOW), "a new window starts afresh");
    }

    #[test]
    fn a_reader_past_its_deadline_times_out_instead_of_waiting() {
        use std::io::Read;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let mut reader = DeadlineReader { stream: &mut server, deadline: Instant::now() };
        let err = reader.read(&mut [0u8; 8]).expect_err("the deadline has passed");
        assert_eq!(err.kind(), ErrorKind::TimedOut);
    }
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
        // Port 0 (ephemeral) keeps a test suite from fighting over a fixed port; still loopback.
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
        // A timing test would be flaky. Assert instead what constant time exists for: no prefix of a
        // token is ever accepted at any length, so there is no early-accept path to time.
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
