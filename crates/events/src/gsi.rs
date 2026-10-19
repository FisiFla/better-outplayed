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
//! 2. A route per game, and a token per game. A game posts to its own path (`/gsi/cs2`,
//!    `/gsi/dota2`); a path no game serves is a `404`, and the token that is expected on
//!    that path is that game's alone. The token is checked in constant time, and checked
//!    *before the body is parsed*. It travels in the payload's `auth` block (GSI has no
//!    header), and a request with a missing or different token is refused with `403` without
//!    the body ever being turned into a document ([`token_matches`], [`payload::auth_token`]).
//! 3. No attacker-controlled content in the log. A refused or unreadable request is counted,
//!    and the log line says how many and what kind; the body, headers and token are never
//!    written.
//!
//! The game is told apart by the route first (each game posts to its own path), then by
//! payload (app id, then provider name; see [`derive::Game`]).
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
//! * **Scope.** Each game has its own path and its own token ([`GsiGameBinding`]), and a
//!   binding can be pinned to an app id ([`GsiConfig::pinned_to`]). A token lifted from a
//!   Counter-Strike installation is refused on Dota 2's path, and a payload that names
//!   another app id is refused even on the path it was lifted for.
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
//! [`GsiListener::accepts_token`] answers whether the token in a file is one of the tokens in
//! force, and [`GsiListener::port`] answers the port the game must be told about. The shell
//! should read both from the running listener rather than from the file.

pub mod derive;
pub mod http;
pub mod integration;
pub mod payload;

pub use derive::{Game, Tracker};
pub use integration::{
    game_path, generate_token, integration_cfg, integration_cfg_dota, load_or_create_token,
    CS2_APP_ID, DEFAULT_PATH, DOTA2_APP_ID, TOKEN_BYTES,
};
pub use payload::{AuthToken, GsiSnapshot};

use crate::wire::write_response;
use crate::{EventSink, GameEvent};
use anyhow::{bail, Context, Result};
use std::fmt;
use std::io::{ErrorKind, Read};
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

/// One game the listener serves: the path it posts to, its own token, and the app id
/// its payloads must carry.
///
/// The binding is what scopes a token. The token is written into the game's `cfg`
/// directory, where any local account that can read the installation can read it, so a
/// token lifted from one game's file is only ever good for the path it was lifted for —
/// and, when the binding names an app id, only for that game's payloads.
#[derive(Clone, PartialEq, Eq)]
pub struct GsiGameBinding {
    /// The path the game posts to, e.g. `/gsi/cs2`. Anything else is a 404.
    pub path: String,
    /// The token for this game alone. Never logged, never printed.
    pub token: String,
    /// Valve's app id: 730 for Counter-Strike, 570 for Dota 2. `None` accepts any.
    pub app_id: Option<u32>,
}

impl GsiGameBinding {
    /// A binding for one game.
    pub fn new(path: impl Into<String>, token: impl Into<String>, app_id: Option<u32>) -> Self {
        Self {
            path: path.into(),
            token: token.into(),
            app_id,
        }
    }
}

/// The token is a credential, so it is never printed: the `Debug` of a binding shows the
/// path and the app id, and says the token is there without saying what it is.
impl fmt::Debug for GsiGameBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GsiGameBinding")
            .field("path", &self.path)
            .field("token", &"<redacted>")
            .field("app_id", &self.app_id)
            .finish()
    }
}

/// One game a listener serves, as the listener reports it: the path and the app id, and
/// never the token. The crate does not hand a token back out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GsiGameInfo {
    /// The path the game posts to.
    pub path: String,
    /// The app id the binding is pinned to, if it is pinned to one.
    pub app_id: Option<u32>,
}

/// Everything the listener needs.
#[derive(Clone, PartialEq, Eq)]
pub struct GsiConfig {
    /// The port from `[events] gsi_port`. Zero means an ephemeral port: the OS picks one
    /// for the session, and the `.cfg` the game is given must name the port the listener
    /// actually bound ([`GsiListener::port`]). A fixed port can be squatted on before the
    /// application starts, and a process squatting on it receives the game's POSTs — and
    /// with them the token.
    pub port: u16,
    /// The games this listener serves. Must not be empty.
    pub games: Vec<GsiGameBinding>,
}

/// The tokens are credentials, so they are never printed: the `Debug` of a configuration
/// shows the port and the games, and each game's `Debug` redacts its token.
impl fmt::Debug for GsiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GsiConfig")
            .field("port", &self.port)
            .field("games", &self.games)
            .finish()
    }
}

impl GsiConfig {
    /// The production configuration: the default path, and the caller's token.
    pub fn new(port: u16, token: impl Into<String>) -> Self {
        Self {
            port,
            games: vec![GsiGameBinding::new(DEFAULT_PATH, token, None)],
        }
    }

    /// The same listener, serving one game only.
    ///
    /// A payload whose `provider.appid` is another game's is refused with `403`, so a token
    /// lifted from one game's installed `.cfg` cannot forge the other's events. The game is
    /// named by Valve's own app id: 730 for Counter-Strike, 570 for Dota 2.
    ///
    /// The app id is set on the first (and, in practice, only) binding: a listener serving
    /// two games is not pinned to one.
    pub fn pinned_to(mut self, app_id: u32) -> Self {
        if let Some(first) = self.games.first_mut() {
            first.app_id = Some(app_id);
        }
        self
    }

    /// The same listener, serving exactly one game: the given path, token and app id.
    ///
    /// The bindings are replaced rather than added to, so this reads as "this listener
    /// serves that one game" rather than as an addition to whatever it served before.
    pub fn with_game(
        mut self,
        path: impl Into<String>,
        token: impl Into<String>,
        app_id: Option<u32>,
    ) -> Self {
        self.games = vec![GsiGameBinding::new(path, token, app_id)];
        self
    }

    /// The same listener, serving one more game: the given path, token and app id.
    pub fn add_game(
        mut self,
        path: impl Into<String>,
        token: impl Into<String>,
        app_id: Option<u32>,
    ) -> Self {
        self.games.push(GsiGameBinding::new(path, token, app_id));
        self
    }

    /// The binding that serves `path`, if this listener serves it.
    ///
    /// The path is matched exactly: a request for a path no game serves is a 404, and a
    /// request for one that is served is checked against that game's own token.
    pub fn game(&self, path: &str) -> Option<&GsiGameBinding> {
        self.games.iter().find(|game| game.path == path)
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
    /// The tokens in force, one per game, so that a caller holding a token file can ask
    /// whether the file is one this listener accepts. The struct deliberately has no
    /// `Debug`: the tokens are never printed.
    tokens: Vec<String>,
    /// The games this listener serves, without their tokens: the crate never hands a token
    /// back out, so the paths and the app ids are all a caller can be told.
    games: Vec<GsiGameInfo>,
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

    /// The games this listener serves, WITHOUT their tokens: the crate never hands a token
    /// back out.
    ///
    /// A caller that has to write one `.cfg` per game needs the path each game posts to
    /// and the app id its binding is pinned to; the token it already has.
    pub fn games(&self) -> Vec<GsiGameInfo> {
        self.games.clone()
    }

    /// Whether `candidate` is a token this listener accepts.
    ///
    /// The desktop shell reads the token file on its own to write the `.cfg`, and the
    /// listener read it once when it started. If the file has been replaced since, the two
    /// disagree and every POST is refused until the application restarts. This is how the
    /// shell can notice: it asks, instead of guessing from a file that may be stale. The
    /// comparison is constant time, like every other one of them, and the token is never
    /// handed back out of the crate.
    ///
    /// The listener serves one game per path, each with its own token, so the answer is
    /// "is this the token of any of them".
    pub fn accepts_token(&self, candidate: &str) -> bool {
        // Every token is compared, and the results are combined without an early exit: the
        // answer must not depend on which game the candidate belongs to.
        let mut accepted = false;
        for expected in &self.tokens {
            accepted |= token_equals(expected, candidate);
        }
        accepted
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
    if config.games.is_empty() {
        // A listener that serves nothing answers every request with a 404, and looks like a
        // broken installation rather than a misconfigured one.
        bail!("the GSI listener needs at least one game to serve");
    }
    for game in &config.games {
        if game.token.is_empty() {
            // Refused at startup rather than failing every request later: a listener that cannot
            // authenticate anything should not be running.
            bail!(
                "the GSI listener needs a token for {}; generate one and put it in the game's cfg file",
                game.path
            );
        }
    }
    for (n, game) in config.games.iter().enumerate() {
        if config.games[..n]
            .iter()
            .any(|other| other.path == game.path)
        {
            // Two games on one path would share a route, and with it a token: the binding is
            // what scopes a token, so it has to be one game per path.
            bail!(
                "the GSI listener serves {} twice; each game needs its own path",
                game.path
            );
        }
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
    let paths = config
        .games
        .iter()
        .map(|game| game.path.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    tracing::info!("the GSI listener is up on http://{addr} (loopback only), serving {paths}");

    let tokens = config.games.iter().map(|game| game.token.clone()).collect();
    let games = config
        .games
        .iter()
        .map(|game| GsiGameInfo {
            path: game.path.clone(),
            app_id: game.app_id,
        })
        .collect();
    let thread = std::thread::Builder::new()
        .name("localplay-gsi".to_string())
        .spawn(move || serve(listener, &config, &sink, &flag))
        .context("spawning the GSI thread")?;

    Ok(GsiListener {
        stop,
        thread: Some(thread),
        addr,
        tokens,
        games,
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

/// How much is taken from the socket at a time while the route is read.
///
/// The same size the HTTP framing takes ([`crate::wire`]). A request that is refused before
/// its body is read leaves the rest of the body in the socket, and the client sees a reset
/// instead of the answer; taking the same amount here keeps the socket in the state the
/// framing would have left it in.
const ROUTE_CHUNK: usize = 4 * 1024;

/// The request's target, and the bytes read to find it.
///
/// The route has to be known before the body is read: the path decides which game's token
/// the body is checked against. The target is in the request's first line, so the first
/// line is read here and the bytes taken are replayed to the reader that frames the rest
/// of the request.
fn peek_target(stream: &mut impl Read) -> Result<(String, Vec<u8>), http::Reject> {
    let mut prefix = Vec::new();
    let mut chunk = [0u8; ROUTE_CHUNK];
    loop {
        let read = stream
            .read(&mut chunk)
            .map_err(|_| http::Reject::Malformed)?;
        if read == 0 {
            // The peer closed before the first line was complete.
            return Err(http::Reject::Malformed);
        }
        prefix.extend_from_slice(&chunk[..read]);
        if let Some(end) = prefix.iter().position(|byte| *byte == b'\n') {
            let line = String::from_utf8_lossy(&prefix[..end]);
            let line = line.trim_end_matches('\r');
            return match request_target(line) {
                Some(target) => Ok((target, prefix)),
                None => Err(http::Reject::Malformed),
            };
        }
        if prefix.len() > crate::wire::MAX_HEAD_BYTES {
            return Err(http::Reject::Malformed);
        }
    }
}

/// The target of a request line, or `None` when the line is not one.
///
/// The line is read the way [`crate::wire`] reads it: a method, a target and a version,
/// separated by single spaces.
fn request_target(line: &str) -> Option<String> {
    let mut parts = line.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if method.is_empty() || target.is_empty() || !version.starts_with("HTTP/") {
        return None;
    }
    Some(target.to_string())
}

/// Serve one connection.
///
/// The order of the gates is the point of this function: the route is resolved, the request
/// is framed, the token is looked for in the raw body, and only a body that carries the
/// right token is ever turned into a document. A peer that cannot authenticate — a web page
/// in the user's browser can `POST` here with a `text/plain` content type, which is a CORS
/// "simple request" and is not preflighted — therefore cannot make the recorder parse
/// megabytes.
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
    // The route first: the path says which game is posting, and the game says which token
    // the body has to carry. A path no game serves is a 404, and the body is not even read.
    let (target, prefix) = match peek_target(&mut reader) {
        Ok(peeked) => peeked,
        Err(reject) => {
            lock(state).counters.refused(reject);
            let _ = write_response(&mut stream, reject.status());
            return;
        }
    };
    // The path is what routes, and nothing in a query is ever read.
    let path = target.split('?').next().unwrap_or_default();
    let Some(game) = config.game(path) else {
        lock(state).counters.refused(http::Reject::NotFound);
        let _ = write_response(&mut stream, http::Reject::NotFound.status());
        return;
    };
    // The bytes the route was read from are replayed, so the request is framed as it would
    // have been without the peek.
    let mut replay = std::io::Cursor::new(prefix).chain(&mut reader);
    let request = match http::read_request(&mut replay, &game.path) {
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
    if !token_equals(&game.token, &presented) {
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
    if !token_matches(&game.token, &snapshot) {
        lock(state).counters.refused_token();
        let _ = write_response(&mut stream, 403);
        return;
    }
    if let Some(app_id) = game.app_id {
        if snapshot.provider.appid != app_id {
            lock(state).counters.refused_game(app_id, &game.path);
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
    fn refused_game(&mut self, app_id: u32, path: &str) {
        self.refused_games += 1;
        if self.refused_games == 1 {
            tracing::warn!(
                "the GSI listener rejected a payload for another game: {path} serves app id \
                 {app_id} only"
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

    #[test]
    fn a_listener_serving_two_games_accepts_the_token_of_either_of_them() {
        // The shell asks whether the token it read is one in force: with a token per game,
        // the answer is "is it the token of any of them".
        let (sink, _rx) = std::sync::mpsc::channel();
        let cs2 = "0123456789abcdef0123456789abcdef"; // gitleaks:allow
        let dota = "fedcba9876543210fedcba9876543210"; // gitleaks:allow
        let listener = spawn(
            GsiConfig::new(0, cs2)
                .with_game("/gsi/cs2", cs2, Some(730))
                .add_game("/gsi/dota2", dota, Some(570)),
            sink,
        )
        .expect("the listener starts");

        assert!(listener.accepts_token(cs2), "the first game's token");
        assert!(listener.accepts_token(dota), "the second game's token");
        assert!(
            !listener.accepts_token("0123456789abcdef0123456789abcdee"),
            "one byte off is not"
        );
        assert!(
            !listener.accepts_token(""),
            "and an empty one is not a token"
        );
    }

    #[test]
    fn a_configuration_never_prints_a_token() {
        // The token is a credential, and a `{:?}` ends up in a log line sooner or later.
        let token = "0123456789abcdef0123456789abcdef"; // gitleaks:allow
        let config = GsiConfig::new(45671, token).pinned_to(730);

        let printed = format!("{config:?}");
        assert!(
            !printed.contains(token),
            "the token must not be printed: {printed}"
        );
        assert!(
            printed.contains("<redacted>"),
            "and it says there is one: {printed}"
        );
        assert!(printed.contains("45671"), "the port is printed: {printed}");
        assert!(
            printed.contains(DEFAULT_PATH),
            "the path is printed: {printed}"
        );
        assert!(printed.contains("730"), "the app id is printed: {printed}");

        let binding = GsiGameBinding::new("/gsi/cs2", token, Some(730));
        let printed = format!("{binding:?}");
        assert!(
            !printed.contains(token),
            "a binding prints no token either: {printed}"
        );
        assert!(
            printed.contains("/gsi/cs2"),
            "the path is printed: {printed}"
        );
    }

    #[test]
    fn a_configuration_serves_the_games_it_was_given() {
        // The default: one game, on the default path, with no app id to check.
        let one = GsiConfig::new(45671, "token");
        assert_eq!(one.port, 45671);
        assert_eq!(one.games.len(), 1);
        assert_eq!(one.games[0].path, DEFAULT_PATH);
        assert_eq!(one.games[0].app_id, None);
        assert!(one.game(DEFAULT_PATH).is_some());
        assert!(one.game("/gsi/cs2").is_none());

        // Pinned: the app id of the first (and only) binding.
        let pinned = one.clone().pinned_to(730);
        assert_eq!(pinned.games[0].app_id, Some(730));
        assert_eq!(
            pinned.game(DEFAULT_PATH).expect("the game").app_id,
            Some(730)
        );

        // Two games: one path and one token each.
        let two = GsiConfig::new(45671, "cs2-token")
            .with_game("/gsi/cs2", "cs2-token", Some(730))
            .add_game("/gsi/dota2", "dota-token", Some(570));
        assert_eq!(two.games.len(), 2);
        assert_eq!(two.game("/gsi/cs2").expect("cs2").token, "cs2-token");
        assert_eq!(two.game("/gsi/dota2").expect("dota2").token, "dota-token");
        assert_eq!(two.game("/gsi/dota2").expect("dota2").app_id, Some(570));
        assert!(two.game(DEFAULT_PATH).is_none());

        // `with_game` replaces what was there; `add_game` adds to it.
        let replaced = two.with_game("/gsi/only", "only-token", None);
        assert_eq!(replaced.games.len(), 1);
        assert_eq!(replaced.games[0].path, "/gsi/only");
    }

    #[test]
    fn a_listener_that_cannot_authenticate_anything_refuses_to_start() {
        // No games at all: a listener that serves nothing answers every request with a 404.
        let err = spawn(
            GsiConfig {
                port: 0,
                games: Vec::new(),
            },
            std::sync::mpsc::channel().0,
        )
        .map(|_| ())
        .expect_err("no game");
        assert!(format!("{err:#}").contains("game"), "got: {err:#}");

        // A game without a token: nothing it posts could be authenticated.
        let err = spawn(
            GsiConfig::new(0, "token").add_game("/gsi/dota2", "", Some(570)),
            std::sync::mpsc::channel().0,
        )
        .map(|_| ())
        .expect_err("no token");
        assert!(format!("{err:#}").contains("token"), "got: {err:#}");

        // Two games on one path: they would share a route, and with it a token.
        let err = spawn(
            GsiConfig::new(0, "token").add_game(DEFAULT_PATH, "other-token", Some(730)),
            std::sync::mpsc::channel().0,
        )
        .map(|_| ())
        .expect_err("one path twice");
        assert!(format!("{err:#}").contains("path"), "got: {err:#}");
    }

    #[test]
    fn the_route_is_the_target_of_the_request_line() {
        use std::io::Cursor;

        let raw = b"POST /gsi/cs2 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n{}";
        let (target, prefix) = peek_target(&mut Cursor::new(raw.to_vec())).expect("a request line");
        assert_eq!(target, "/gsi/cs2");
        assert!(
            prefix.starts_with(b"POST /gsi/cs2 HTTP/1.1\r\n"),
            "the bytes taken are replayed to the reader that frames the rest: {prefix:?}"
        );

        // A line that is not a request line is refused rather than guessed at.
        for raw in [
            b"POST /gsi\r\n".to_vec(),
            b"GET\r\n".to_vec(),
            b"POST  /gsi HTTP/1.1\r\n".to_vec(),
            b"garbage\r\n".to_vec(),
        ] {
            assert!(
                peek_target(&mut Cursor::new(raw.clone())).is_err(),
                "{raw:?} is not a request line"
            );
        }
    }
}
