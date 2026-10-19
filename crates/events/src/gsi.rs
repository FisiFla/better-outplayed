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
//! 2. A token, checked in constant time, and checked *before the body is parsed*. It travels
//!    in the payload's `auth` block (GSI has no header), and a request with a missing or
//!    different token is refused with `403` without the body ever being turned into a
//!    document ([`token_matches`], [`payload::auth_token`]).
//! 3. No attacker-controlled content in the log. A refused or unreadable request is counted,
//!    and the log line says how many and what kind; the body, headers and token are never
//!    written.
//!
//! The game is told apart only by payload (app id, then provider name; see [`derive::Game`]).
//! The listener cannot react to the game exiting: a killed process sends no final POST, so
//! `GameEnd` is only derived from a payload that says so (`map.phase == "gameover"`, Dota's
//! `POST_GAME`). That is why League's end-of-game signal (the endpoint going quiet) differs.
//!
//! # The accept loop never blocks on a connection
//!
//! A peer that opens a connection and then says nothing holds a reader for the whole
//! request deadline. The loop used to serve one connection at a time, so a handful of such
//! connections kept the game's own POSTs from being accepted at all: the recorder kept
//! running and the feature it exists for was silent, which is a denial of service that looks
//! exactly like a game that is not playing. The loop now accepts and hands the connection to
//! one of [`WORKERS`] readers, and the queue in front of them ([`MAX_QUEUED`]) is the
//! in-flight bound: past it, the connection is answered `429` unread.
//!
//! The readers fold the payloads into the tracker in the order the connections were
//! accepted. A payload that arrives after a later one is dropped rather than folded in: the
//! baseline would move backwards, and the next delta would be reported as a jump. That
//! needs two of the game's own POSTs to be in flight at once, which the game does not do —
//! it waits for the answer — so it is a corner rather than a habit.
//!
//! # What the token does not protect against
//!
//! The token is written into the game's `cfg` directory, and on Windows that directory is
//! readable by the `Users` group. Any local account that can read the game installation can
//! therefore forge events: post a payload with the right token and a scoreboard that says
//! the player got a kill, and the recorder cuts a clip of nothing. The token cannot be
//! hidden — the game has to read it — so the defences are scope and freshness, not secrecy:
//!
//! * **Scope.** [`GsiConfig::pinned_to`] pins the listener to one game, so a token lifted
//!   from a Counter-Strike installation cannot forge Dota events, and the other way round.
//! * **Freshness.** A port of `0` is an ephemeral port, chosen by the OS for the session, so
//!   a token captured from the wire is only good for the session it was captured in and a
//!   process squatting on the port cannot learn the token at all. The `.cfg` the game is
//!   given must name the port the listener actually bound ([`GsiListener::port`]).
//!
//! What is left is the residual risk: a local account that can read the game folder can
//! forge events for the game the listener is pinned to, for as long as the installation
//! keeps the token. Closing that needs an ACL on the `.cfg`, or not writing the token into
//! the game folder at all, and neither is this crate's to do.
//!
//! # The token file, and its second reader
//!
//! The listener reads the token **once**, when it starts. The desktop shell's installer
//! reads the same file **independently**, whenever the user presses "install". If the file
//! is deleted or replaced while a recording is running, the two disagree: the listener keeps
//! the token it started with, the installer writes the new one into the `.cfg`, and every
//! POST is answered `403` until the application restarts. This crate cannot fix that — the
//! installer is not its code — but it can make the divergence visible:
//! [`GsiListener::accepts_token`] answers whether the token in a file is the one in force,
//! and [`GsiListener::port`] answers the port the game must be told about. The shell should
//! read both from the running listener rather than from the file.

pub mod derive;
pub mod http;
pub mod integration;
pub mod payload;

pub use derive::{Game, Tracker};
pub use integration::{
    generate_token, integration_cfg, integration_cfg_dota, load_or_create_token, DEFAULT_PATH,
    TOKEN_BYTES,
};
pub use payload::{AuthToken, GsiSnapshot};

use crate::wire::write_response;
use crate::{EventSink, GameEvent};
use anyhow::{bail, Context, Result};
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
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
///
/// Two seconds, not five: the game answers in milliseconds over loopback, and this is also
/// the longest a trickling peer can hold one of the [`WORKERS`] readers, which is a reader the
/// game's own POST then has to wait for.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(2);

/// How many connections are read and answered at once.
///
/// The accept loop must never block on a connection. A peer that trickles one byte at a time
/// would otherwise hold the loop for the whole request deadline — a hundred such
/// connections, and the game's own POSTs are not accepted for minutes — and the recorder
/// would keep running, silently, while the feature it exists for is dead. Four readers is
/// plenty for one client posting a handful of times a second, and few enough that a flood
/// of connections cannot become a flood of threads.
pub const WORKERS: usize = 4;

/// How many accepted connections may wait for a reader.
///
/// This is the in-flight bound: a reader plus a queue of this many is the most the listener
/// will hold at once, and the next connection is answered `429` unread. Without it, a
/// process opening sockets as fast as it can would make the recorder hold thousands of
/// them.
pub const MAX_QUEUED: usize = 4;

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
        Self {
            window_start: now,
            count: 0,
        }
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
        self.stream
            .set_read_timeout(Some(left.min(REQUEST_TIMEOUT)))?;
        self.stream.read(buf)
    }
}

/// Everything the listener needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GsiConfig {
    /// The port from `[events] gsi_port`. Zero means an ephemeral port: the OS picks one
    /// for the session, and the `.cfg` the game is given must name the port the listener
    /// actually bound ([`GsiListener::port`]). A fixed port can be squatted on before the
    /// application starts, and a process squatting on it receives the game's POSTs — and
    /// with them the token.
    pub port: u16,
    /// The token from the user's `gamestate_integration_localplay.cfg`. Never logged.
    pub token: String,
    /// The path the game is told to POST to; anything else is a 404.
    pub path: String,
    /// The game this listener serves, when it is pinned to one. `None` serves both, which is
    /// what a single port for two games means; see [`GsiConfig::pinned_to`].
    pub app_id: Option<u32>,
}

impl GsiConfig {
    /// The production configuration: the default path, and the caller's token.
    pub fn new(port: u16, token: impl Into<String>) -> Self {
        Self {
            port,
            token: token.into(),
            path: DEFAULT_PATH.to_string(),
            app_id: None,
        }
    }

    /// The same listener, serving one game only.
    ///
    /// A payload whose `provider.appid` is another game's is refused with `403`, so a token
    /// lifted from one game's installed `.cfg` cannot forge the other's events. The game is
    /// named by Valve's own app id: 730 for Counter-Strike, 570 for Dota 2.
    pub fn pinned_to(mut self, app_id: u32) -> Self {
        self.app_id = Some(app_id);
        self
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

/// Whether `presented` is the expected token, compared in constant time.
///
/// Fails closed: an empty expected token is a programming error and matches nothing, since
/// it would accept every payload declaring an empty one. A token of a different length does
/// not match either, so the comparison is over the whole value and there is no prefix of a
/// token that is ever accepted.
fn token_equals(expected: &str, presented: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    presented.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// Whether `snapshot` carries the expected token.
///
/// Fails closed: no token, a token of a different length, and a right-length token differing
/// by one byte are all `false`. The comparison is constant time ([`subtle`]) so a local
/// process cannot learn the token byte by byte from rejection timing. An empty expected token
/// is a programming error and is also refused, since it would accept every payload declaring
/// an empty one.
pub fn token_matches(expected: &str, snapshot: &GsiSnapshot) -> bool {
    snapshot
        .auth
        .get("token")
        .is_some_and(|presented| token_equals(expected, presented))
}

/// A running listener.
pub struct GsiListener {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    addr: SocketAddr,
    /// The token in force, so that a caller holding a token file can ask whether the file is
    /// the one this listener accepts. The struct deliberately has no `Debug`: the token is
    /// never printed.
    token: String,
}

impl GsiListener {
    /// The address actually bound (an ephemeral port in tests); what matters is that its IP is
    /// loopback.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The port the listener is bound on.
    ///
    /// The `.cfg` the game is given must name this port and not a configured one: a port
    /// that was already taken, or an ephemeral one, are both ways for the game and the
    /// listener to disagree, and the game would then post into the void.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Whether `candidate` is the token this listener accepts.
    ///
    /// The desktop shell reads the token file on its own to write the `.cfg`, and the
    /// listener read it once when it started. If the file has been replaced since, the two
    /// disagree and every POST is refused until the application restarts. This is how the
    /// shell can notice: it asks, instead of guessing from a file that may be stale. The
    /// comparison is constant time, like every other one of them, and the token is never
    /// handed back out of the crate.
    pub fn accepts_token(&self, candidate: &str) -> bool {
        token_equals(&self.token, candidate)
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
    let addr = listener
        .local_addr()
        .context("reading the listener's address")?;
    listener
        .set_nonblocking(true)
        .context("making the listener non-blocking")?;

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    tracing::info!(
        "the GSI listener is up on http://{addr}{} (loopback only)",
        config.path
    );

    let token = config.token.clone();
    let thread = std::thread::Builder::new()
        .name("localplay-gsi".to_string())
        .spawn(move || serve(listener, &config, &sink, &flag))
        .context("spawning the GSI thread")?;

    Ok(GsiListener {
        stop,
        thread: Some(thread),
        addr,
        token,
    })
}

/// One accepted connection, and the order it was accepted in.
struct Job {
    stream: TcpStream,
    /// The accept order. The tracker is folded in this order, so a payload that arrives
    /// after a later one is dropped rather than moving the baseline backwards.
    seq: u64,
}

/// What the listener keeps between connections.
#[derive(Debug, Default)]
struct State {
    /// The derivation's own state: the baseline the next payload is compared against.
    tracker: Tracker,
    /// How many requests of each kind the listener has seen.
    counters: Counters,
    /// The accept order of the last payload folded into the tracker.
    last_seq: u64,
}

/// The lock, ignoring poisoning.
///
/// A connection that panics must not take the listener with it: the poison is the record of
/// that panic, and the state behind it is still the truth about the game.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The accept loop and its readers.
///
/// The loop accepts and hands the connection to a reader; it never reads a request itself,
/// so a peer that trickles one byte at a time cannot keep the game's POSTs from being
/// accepted. The readers are [`WORKERS`] threads over a queue of [`MAX_QUEUED`], and the
/// queue's being full is the in-flight bound: the next connection is answered `429` unread.
fn serve(listener: TcpListener, config: &GsiConfig, sink: &EventSink, stop: &Arc<AtomicBool>) {
    let (jobs, queue) = sync_channel::<Job>(MAX_QUEUED);
    let queue = Arc::new(Mutex::new(queue));
    let state = Arc::new(Mutex::new(State::default()));

    let mut readers = Vec::with_capacity(WORKERS);
    for n in 0..WORKERS {
        let queue = Arc::clone(&queue);
        let state = Arc::clone(&state);
        let config = config.clone();
        let sink = sink.clone();
        let flag = Arc::clone(stop);
        let reader = std::thread::Builder::new()
            .name(format!("localplay-gsi-{n}"))
            .spawn(move || read_loop(&queue, &config, &sink, &state, &flag))
            .with_context(|| format!("spawning the GSI reader thread {n}"));
        match reader {
            Ok(handle) => readers.push(handle),
            Err(err) => {
                // Without a reader there is nothing to answer with, so the listener stops
                // rather than accepting connections it cannot serve.
                tracing::warn!("the GSI listener could not start a reader: {err:#}");
                drop(jobs);
                return;
            }
        }
    }

    let mut limiter = RateLimiter::new(Instant::now());
    let mut seq: u64 = 0;
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                // `_peer` is not logged: on a loopback bind it is `127.0.0.1` by definition.
                if !limiter.allow(Instant::now()) {
                    let mut stream = stream;
                    lock(&state).counters.rate_limited();
                    turn_away(&mut stream);
                    continue;
                }
                seq += 1;
                let job = Job { stream, seq };
                match jobs.try_send(job) {
                    Ok(()) => {}
                    Err(TrySendError::Full(job)) | Err(TrySendError::Disconnected(job)) => {
                        // The readers are all busy and the queue is full: the connection is
                        // refused unread rather than held.
                        let mut stream = job.stream;
                        lock(&state).counters.busy();
                        turn_away(&mut stream);
                    }
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

    // The readers wait on the queue, and the queue ends when the sender is dropped.
    drop(jobs);
    for reader in readers {
        let _ = reader.join();
    }
    tracing::debug!("the GSI listener stopped");
}

/// One reader: take a connection, answer it, take the next.
fn read_loop(
    queue: &Arc<Mutex<Receiver<Job>>>,
    config: &GsiConfig,
    sink: &EventSink,
    state: &Arc<Mutex<State>>,
    stop: &Arc<AtomicBool>,
) {
    loop {
        let job = {
            // The lock is held across the wait, which serialises the hand-out and not the
            // work: a reader that has a connection is not holding it.
            let queue = lock(queue);
            match queue.recv() {
                Ok(job) => job,
                // The listener is shutting down.
                Err(_) => break,
            }
        };
        if stop.load(Ordering::SeqCst) {
            break;
        }
        serve_one(job, config, sink, state, stop);
    }
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

/// Serve one connection.
///
/// The order of the gates is the point of this function: the request is framed, the token is
/// looked for in the raw body, and only a body that carries the right token is ever turned
/// into a document. A peer that cannot authenticate — a web page in the user's browser can
/// `POST` here with a `text/plain` content type, which is a CORS "simple request" and is not
/// preflighted — therefore cannot make the recorder parse megabytes.
fn serve_one(
    job: Job,
    config: &GsiConfig,
    sink: &EventSink,
    state: &Arc<Mutex<State>>,
    stop: &Arc<AtomicBool>,
) {
    let Job { mut stream, seq } = job;
    // `accept` inherits the listener's non-blocking flag on macOS/*BSD (not Linux or Windows), and
    // a non-blocking socket ignores the read/write timeouts set below: every read with no buffered
    // data fails with `WouldBlock`, which `http` reads as a malformed request (400) instead of the
    // 403/200 the client should see. Put the accepted socket back into blocking mode first so the
    // timeouts are the connection's real bound. A no-op on Windows and Linux.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_nodelay(true);

    let mut reader = DeadlineReader {
        stream: &mut stream,
        deadline: Instant::now() + REQUEST_DEADLINE,
    };
    let request = match http::read_request(&mut reader, &config.path) {
        Ok(request) => request,
        Err(reject) => {
            lock(state).counters.refused(reject);
            let _ = write_response(&mut stream, reject.status());
            return;
        }
    };

    // The token, read out of the raw body without parsing the rest of it. The block is the
    // first thing the game sends, so this costs a few bytes however large the body is.
    let presented = match payload::auth_token(&request.body) {
        payload::AuthToken::Present(token) => token,
        payload::AuthToken::Absent => {
            lock(state).counters.refused_token();
            let _ = write_response(&mut stream, 403);
            return;
        }
        payload::AuthToken::Unreadable => {
            lock(state).counters.malformed(request.body.len());
            let _ = write_response(&mut stream, 400);
            return;
        }
    };
    if !token_equals(&config.token, &presented) {
        lock(state).counters.refused_token();
        let _ = write_response(&mut stream, 403);
        return;
    }

    // The body is known to carry the right token, so it is worth parsing.
    let snapshot = match payload::parse(&request.body) {
        Ok(snapshot) => snapshot,
        Err(_) => {
            // Deliberately silent about what was wrong: the body is attacker-controlled before the token
            // gate runs and `serde_json`'s message can quote it. The size and count suffice to debug an
            // install.
            lock(state).counters.malformed(request.body.len());
            let _ = write_response(&mut stream, 400);
            return;
        }
    };

    // The document is the authority on the token, not the look above: a body carrying two
    // `auth` blocks is parsed into the last one, and that is the one that counts.
    if !token_matches(&config.token, &snapshot) {
        lock(state).counters.refused_token();
        let _ = write_response(&mut stream, 403);
        return;
    }
    if let Some(app_id) = config.app_id {
        if snapshot.provider.appid != app_id {
            lock(state).counters.refused_game(app_id);
            let _ = write_response(&mut stream, 403);
            return;
        }
    }

    // Fold it in, in the order the connections were accepted. The events are emitted under
    // the same lock, so they leave in that order too.
    {
        let mut state = lock(state);
        if seq <= state.last_seq {
            // A payload that arrived after a later one: folding it in would move the
            // baseline backwards, and the next delta would be reported as a jump.
            tracing::debug!("the GSI listener dropped a payload that arrived out of order");
            return;
        }
        state.last_seq = seq;
        let events = state.tracker.observe(&snapshot);
        state.counters.accepted();
        for event in events {
            log_event(&event);
            // A disconnected sink means the driver is gone.
            if sink.send(event).is_err() {
                stop.store(true, Ordering::SeqCst);
                return;
            }
        }
    }
    // Answer last: a client that reads the response knows the payload was taken.
    let _ = write_response(&mut stream, 200);
}

/// One line per derived event at the level it deserves: game transitions `info`, each kill
/// `debug`. The payload is written by [`derive`], never echoed from the request, so quoting it
/// is safe.
fn log_event(event: &GameEvent) {
    match event.kind {
        crate::EventKind::GameStart => {
            tracing::info!(
                "a GSI game is being watched: {}",
                event.payload.as_deref().unwrap_or("")
            )
        }
        crate::EventKind::GameEnd => tracing::info!("the GSI game is over"),
        kind => tracing::debug!(
            "GSI event: {kind} {}",
            event.payload.as_deref().unwrap_or("")
        ),
    }
}

/// How many requests of each kind the listener has seen, so a condition is stated once and
/// then counted; otherwise anything scanning loopback ports could fill the log.
#[derive(Debug, Default)]
struct Counters {
    accepted: u64,
    refused: u64,
    refused_tokens: u64,
    refused_games: u64,
    malformed: u64,
    rate_limited: u64,
    busy: u64,
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
            tracing::debug!(
                "the GSI listener has rejected {} payloads by token",
                self.refused_tokens
            );
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
            tracing::debug!(
                "the GSI listener has rate-limited {} connections",
                self.rate_limited
            );
        }
    }

    /// A payload that authenticated but is another game's: the token was lifted from the
    /// wrong installed file.
    fn refused_game(&mut self, app_id: u32) {
        self.refused_games += 1;
        if self.refused_games == 1 {
            tracing::warn!(
                "the GSI listener rejected a payload for another game: this listener serves \
                 app id {app_id} only"
            );
        } else {
            tracing::debug!(
                "the GSI listener has rejected {} payloads for another game",
                self.refused_games
            );
        }
    }

    /// A connection refused because every reader was busy: the in-flight bound, not the
    /// per-second one.
    fn busy(&mut self) {
        self.busy += 1;
        if self.busy == 1 {
            tracing::warn!(
                "the GSI listener has {WORKERS} readers and a queue of {MAX_QUEUED}, and is \
                 answering the excess connections with HTTP 429"
            );
        } else {
            tracing::debug!(
                "the GSI listener has refused {} busy connections",
                self.busy
            );
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
            tracing::debug!(
                "the GSI listener has dropped {} unparseable payloads",
                self.malformed
            );
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
        assert!(
            !limiter.allow(t0 + RATE_WINDOW / 2),
            "and so is one later in the same window"
        );
        assert!(
            limiter.allow(t0 + RATE_WINDOW),
            "a new window starts afresh"
        );
    }

    #[test]
    fn a_reader_past_its_deadline_times_out_instead_of_waiting() {
        use std::io::Read;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let mut reader = DeadlineReader {
            stream: &mut server,
            deadline: Instant::now(),
        };
        let err = reader
            .read(&mut [0u8; 8])
            .expect_err("the deadline has passed");
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
            let err = bind(addr)
                .map(|_| ())
                .expect_err("only loopback may be bound");
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
            snapshot(&format!(
                r#"{{"auth":{{"token":"{token}"}},"provider":{{"appid":730}}}}"#
            ))
        };

        assert!(
            token_matches(expected, &with(expected)),
            "the same token is accepted"
        );
        assert!(
            !token_matches(expected, &with("0123456789abcdef0123456789abcdee")),
            "one byte off"
        );
        assert!(
            !token_matches(expected, &with("0123456789abcdef0123456789abcde")),
            "one byte short"
        );
        assert!(
            !token_matches(expected, &with("0123456789abcdef0123456789abcdefg")),
            "one byte long"
        );
        assert!(
            !token_matches(expected, &with("")),
            "an empty token is not a token"
        );
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
                !token_matches(
                    expected,
                    &snapshot(&format!(r#"{{"auth":{{"token":"{prefix}"}}}}"#))
                ),
                "a {end}-byte prefix must not be accepted"
            );
        }
    }

    #[test]
    fn a_listener_without_a_token_refuses_to_start() {
        let (sink, _rx) = std::sync::mpsc::channel();
        let err = spawn(GsiConfig::new(0, ""), sink)
            .map(|_| ())
            .expect_err("no token");
        assert!(format!("{err:#}").contains("token"), "got: {err:#}");
    }

    #[test]
    fn the_listener_reports_the_port_it_bound_and_the_token_in_force() {
        // The `.cfg` the game is given must name the port the listener actually bound, and
        // a shell that reads the token file on its own must be able to tell whether the
        // file it read is the one in force: a file replaced while the recording runs is a
        // divergence the shell can only notice if the listener answers.
        let (sink, _rx) = std::sync::mpsc::channel();
        // A fixed token, not a credential: the digits of pi in hex, the same in every test.
        // The real one is generated from the OS entropy source at run time. `gitleaks:allow`
        // because a hex string assigned to a name containing "token" reads as a leak.
        let token = "0123456789abcdef0123456789abcdef"; // gitleaks:allow
        let listener = spawn(GsiConfig::new(0, token), sink).expect("the listener starts");
        assert_eq!(listener.port(), listener.local_addr().port());
        assert_ne!(listener.port(), 0, "the OS assigned a real port");
        assert!(
            listener.accepts_token(token),
            "the token in force is accepted"
        );
        assert!(
            !listener.accepts_token("0123456789abcdef0123456789abcdee"),
            "one byte off is not"
        );
        assert!(
            !listener.accepts_token(""),
            "and an empty one is not a token"
        );
    }
}
