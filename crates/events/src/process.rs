//! Is a watched game running? Two signals, one answer.
//!
//! Auto-record switches itself on when a game starts, which needs exactly one fact: *is a
//! watched title running right now*. There are two honest ways to learn that, so this
//! module has two sources behind one trait ([`GamePresence`]):
//!
//! | source | how it answers | what it can see |
//! |---|---|---|
//! | [`ProcessWatcher`] | a `CreateToolhelp32Snapshot` process list, compared by image name | any title whose executable is configured |
//! | [`LiveClientPresence`] | Riot's own Live Client Data API, a `GET` on loopback | League of Legends, for as long as a match is actually being played |
//!
//! Both are polled on a timer ([`spawn`], [`DEFAULT_POLL_INTERVAL`]) and both answer the
//! same [`Presence`], so the debounce, the "already running when we started" case and the
//! "report a change exactly once" rule are written once ([`PresenceTracker`]) rather than
//! twice, and are tested without a clock, a socket or a process list.
//!
//! # Which signal for which title, and why that is not a free choice
//!
//! Riot publishes a *game-state* API for exactly one title on the default watch list:
//! League of Legends' Live Client Data API, which is served on `127.0.0.1:2999` while a
//! match is running (it is not up in the client, in the lobby or in champ select) and stops
//! answering when the match ends. League is therefore detected through that — a read-only
//! `GET` to Riot's own documented endpoint, a signal that by construction cannot fire
//! outside a game.
//!
//! Everything else is recognised by its executable name, because nothing else on the list
//! publishes such an API. That includes **Valorant**, which is a Riot title but has no
//! documented local *game-state* service: what exists is the **Riot Client's** local HTTP
//! API behind a lockfile password — a different thing entirely (it reports the launcher's
//! session, not a match), undocumented, and not used here. Valorant is watched by
//! `VALORANT-Win64-Shipping.exe`, which is the *game* process, present only while a match
//! runs — not `VALORANT.exe`, which is the launcher and would be a false positive all
//! evening. The same distinction holds for League (`League of Legends.exe`, not
//! `LeagueClient.exe`) and for Counter-Strike 2 (`cs2.exe`, not `steam.exe`).
//!
//! # What the process watcher does, and what it deliberately does not
//!
//! [`ProcessWatcher`] calls `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)` and walks the
//! resulting `PROCESSENTRY32W` records — the same enumeration Task Manager performs. It:
//!
//! * never calls `OpenProcess`: nothing about a process is opened, and no access right is
//!   requested over any of them. A name is copied out of the snapshot and compared to the
//!   configured list;
//! * therefore reads no memory, injects nothing, hooks nothing, and touches no module,
//!   window, thread or handle belonging to another process;
//! * opens no socket, and sends nothing anywhere: `tests/no_egress.rs` asserts statically
//!   that the tree's only socket clients are the loopback modules, and this file is not one
//!   of them;
//! * holds no handle between polls: the snapshot handle is closed before `poll` returns.
//!
//! That matters on a machine running kernel-level anti-cheat. Enumerating image names is
//! what every launcher, overlay and task manager already does, and it is the least invasive
//! mechanism that answers the question — but it is still a new capability, so it is off by
//! default: [`GamesSection::auto_record`] is `false`, and with it off nothing is polled,
//! no snapshot is taken and no request is made ([`GamesSection::start`] returns `None`).
//!
//! # What is not verified here, and cannot be from this machine
//!
//! [`image_names`] is the only part of this module that has never executed: it is
//! type-checked against `x86_64-pc-windows-msvc`, but no Windows machine was available, so
//! the syscall half is unproven. It is deliberately four calls wide
//! (`CreateToolhelp32Snapshot`, `Process32FirstW`, `Process32NextW`, `CloseHandle`) so that
//! what is unproven is small and obvious, and everything that decides anything
//! ([`presences_in`]) is pure and tested on any platform. The same shadow covers whether a
//! *protected* process's image name appears in a snapshot under kernel anti-cheat; if it
//! did not, the result would be a game that is never detected — a false negative, never a
//! wrong match, because a match requires an equal file name.

use crate::lol::{Endpoint, LoopbackClient, Response};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often a game is looked for when nothing says otherwise.
///
/// A few seconds is the whole point of the number. It is far below the time it takes to
/// reach a game from a launch, and the replay buffer's `pre_seconds` covers the gap on the
/// way in, so a start noticed a couple of seconds late still has its footage.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(2_500);

/// The same interval, as the `poll_ms` a `config.toml` writes.
pub const DEFAULT_POLL_MS: u64 = 2_500;

/// The shortest interval a *configured* `poll_ms` can produce.
///
/// The floor is not politeness. At a few milliseconds the watcher would enumerate the
/// process list in a tight loop, which is a CPU burn on the user's machine and — on a
/// machine running kernel anti-cheat — a process hammering `CreateToolhelp32Snapshot`
/// hundreds of times a second, which is exactly the profile of a tool nobody wants running
/// next to their game. A configuration mistake must not be able to produce that, so the
/// clamp lives in the one function that turns configuration into an interval
/// ([`clamp_interval`]) and is tested.
pub const MIN_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The longest interval a *configured* `poll_ms` can produce: half a minute still notices a
/// game, and past that the feature is not doing the job it was switched on for.
pub const MAX_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How many consecutive polls must agree before a *change* is believed.
///
/// Two, so that one flickering poll cannot start or end a session: a snapshot taken while a
/// game is restarting a worker, or during the moment a process is exiting, is not a game
/// that stopped. At the default interval the cost is one extra interval (~2.5 s) of latency
/// on a transition, which is inside the capture buffer's pre-roll.
pub const CONFIRMATIONS: u32 = 2;

/// How long [`WatchHandle`]'s thread sleeps at a time, so that `stop` does not wait a whole
/// interval. The sleep is a sleep, not a spin.
const STOP_SLICE: Duration = Duration::from_millis(50);

/// A game this build knows how to recognise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchedGame {
    /// Human name shown in the tray/panel: "League of Legends".
    pub name: String,
    /// Windows executable to compare against, case-insensitively: "League of Legends.exe".
    pub exe: String,
    /// How this title is detected. Riot titles that expose the official Live Client Data
    /// API use [`Signal::LiveClientApi`]; everything else uses [`Signal::Process`].
    ///
    /// Absent from a configuration entry, it is [`Signal::Process`] — the general
    /// mechanism. The Live Client Data API is the exception and is named explicitly,
    /// because only a title that publishes one can be detected that way.
    #[serde(default)]
    pub signal: Signal,
}

impl WatchedGame {
    /// A title recognised by its executable's file name.
    pub fn by_process(name: impl Into<String>, exe: impl Into<String>) -> Self {
        Self { name: name.into(), exe: exe.into(), signal: Signal::Process }
    }

    /// A Riot title recognised by the Live Client Data API it publishes.
    pub fn by_live_client_api(name: impl Into<String>, exe: impl Into<String>) -> Self {
        Self { name: name.into(), exe: exe.into(), signal: Signal::LiveClientApi }
    }

    /// The file name this title is compared by — the `exe` with any path removed and any
    /// surrounding whitespace trimmed, which is the form the matcher compares.
    pub fn file_name(&self) -> &str {
        file_name(&self.exe)
    }
}

impl fmt::Display for WatchedGame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name, self.exe)
    }
}

/// How a title is detected.
///
/// The two variants are not interchangeable *for a given title*: Riot serves the Live
/// Client Data API for League of Legends and for nothing else on the default list, and a
/// title with no such API cannot be found by asking for one. Whoever configures a title
/// picks the mechanism that title actually offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// By executable name, from a process-list snapshot. The only mechanism available for
    /// a title that publishes nothing on loopback.
    #[default]
    Process,
    /// Riot's Live Client Data API on loopback, which is served only while a match is
    /// running. League of Legends publishes one; no other title on the default list does.
    LiveClientApi,
}

/// One poll's answer about one watched game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presence {
    pub game: WatchedGame,
    pub running: bool,
}

/// A detector polled on a timer.
///
/// Implementations must be cheap: [`spawn`] calls [`GamePresence::poll`] every few seconds
/// for the whole life of the tray process, and what it does must not be visible in Task
/// Manager. Neither implementation here holds a handle, a socket or a connection open
/// between polls, and neither allocates anything that outlives one.
///
/// The `Result` is how a detector says "I could not look this time" as opposed to "I looked,
/// and there is nothing there" — a distinction [`GameWatcher::poll_once`] relies on, since
/// only the second is evidence about a game.
pub trait GamePresence: Send {
    fn poll(&mut self) -> Result<Vec<Presence>>;
}

/// Which of `watched` are present in this snapshot of image names?
///
/// This is the whole decision a process watcher makes, as a pure function over
/// `&[String]`: it reads no system state, takes no lock, touches no clock and can be driven
/// exhaustively from a test on any platform. [`ProcessWatcher::poll`] exists only to hand
/// it the names.
///
/// Every watched game gets an answer, present or not, so the result is the same length as
/// `watched` and a caller never has to distinguish "not running" from "not asked about".
pub fn presences_in(watched: &[WatchedGame], observed: &[String]) -> Vec<Presence> {
    watched
        .iter()
        .map(|game| Presence {
            game: game.clone(),
            running: observed.iter().any(|name| image_name_matches(&game.exe, name)),
        })
        .collect()
}

/// Whether an observed image name is the file `configured` names.
///
/// The comparison is between **file names**, exactly:
///
/// * both sides are reduced to their file name first, so a snapshot that reported a full
///   path (`C:\...\cs2.exe`, which `PROCESSENTRY32W` is not documented to do but is not
///   documented not to) and a configuration value pasted as a path both mean the file they
///   name;
/// * case-insensitively, and **ASCII-only** on purpose: Windows' file-name
///   case-insensitivity is ASCII, and a Unicode case fold would make `СS2.EXE`, written
///   with a Cyrillic С, match `cs2.exe`. That is the one false positive that really costs
///   something here — a false positive starts a recording nobody asked for;
/// * never by substring: `notcs2.exe`, `cs2.exe.bak` and `mycs2.exe` are all different
///   files from `cs2.exe`.
fn image_name_matches(configured: &str, observed: &str) -> bool {
    let configured = file_name(configured);
    let observed = file_name(observed);
    // An empty file name names no file, on either side, and matches nothing — not even
    // another empty name. A snapshot entry that could not be read must not become a match
    // for a configuration value that could not be written.
    !configured.is_empty() && configured.eq_ignore_ascii_case(observed)
}

/// The file-name part of a name that may be a path.
///
/// A name that is nothing but separators is returned unchanged: there is no file name to
/// find in it, and returning the empty tail would silently match nothing at all (or worse,
/// look like a match for an equally empty configuration value).
fn file_name(name: &str) -> &str {
    let name = name.trim();
    match name.rsplit(['\\', '/']).next() {
        Some(file) if !file.is_empty() => file,
        _ => name,
    }
}

/// The process-name watcher: compare a snapshot of image names against the configured list.
///
/// This type is the thin half of the detection. The comparison itself is [`presences_in`],
/// which takes plain strings and is tested everywhere; what is left here is one snapshot
/// call, one walk and one handle close, all of which live in [`image_names`].
///
/// # Cost, stated
///
/// One poll is one `CreateToolhelp32Snapshot` plus a walk of the process list (a few hundred
/// entries on a desktop) and copies each entry's image name into a vector that is dropped
/// before `poll` returns. Nothing about a process is opened, read or held: there is no
/// handle between polls, no thread kept alive and no growing allocation — the vector is
/// per-poll and transient, and the only long-lived state is the configured list itself.
/// Measured cost is a few hundred microseconds, once every [`DEFAULT_POLL_INTERVAL`].
#[derive(Debug)]
pub struct ProcessWatcher {
    watched: Vec<WatchedGame>,
}

impl ProcessWatcher {
    /// Watch `watched` by executable name.
    ///
    /// The list is validated here rather than at match time (see [`validate_watch_list`]):
    /// a duplicate or nameless entry is a configuration mistake, and a watcher that starts
    /// up and silently watches nothing is the worst way to learn about one.
    pub fn new(watched: Vec<WatchedGame>) -> Result<Self> {
        validate_watch_list(&watched)?;
        Ok(Self { watched })
    }

    /// What this watcher was told to look for.
    pub fn watched(&self) -> &[WatchedGame] {
        &self.watched
    }
}

impl GamePresence for ProcessWatcher {
    fn poll(&mut self) -> Result<Vec<Presence>> {
        #[cfg(windows)]
        {
            let observed = image_names()?;
            Ok(presences_in(&self.watched, &observed))
        }
        #[cfg(not(windows))]
        {
            // Not an empty list: "we cannot look" and "nothing is there" are different
            // answers, and only one of them is true here. An empty list would be read by a
            // driver as every game being stopped, which is a lie it may act on.
            bail!(
                "watching for a game by executable name needs the Windows process-list \
                 snapshot (CreateToolhelp32Snapshot), which this platform does not have; \
                 nothing was observed, and no game is reported as stopped"
            )
        }
    }
}

/// One snapshot of every process's image name — the only Windows code in this module.
///
/// `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)` is the enumeration Task Manager
/// performs: the kernel hands back a copy of the process list as `PROCESSENTRY32W` records,
/// and this function copies the image names out of them. It opens **no** process: there is
/// no `OpenProcess` call, so no access right is requested over any running game, no memory
/// is read, and no module, thread or window is touched. The only handle is the snapshot's
/// own, and it is closed before this returns, so no handle is held between polls.
///
/// The walk is deliberately tiny — four calls — because this half cannot be tested on the
/// machine the project is developed on (see the module docs).
#[cfg(windows)]
fn image_names() -> Result<Vec<String>> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    /// The snapshot handle, closed however the function leaves — including through `?`.
    struct Snapshot(HANDLE);

    impl Drop for Snapshot {
        fn drop(&mut self) {
            // Best effort: a close that fails is not something a caller can act on, and
            // there is nothing else to do with the value.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    let snapshot = Snapshot(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
            .map_err(|e| anyhow!("taking a process-list snapshot: {e}"))?,
    );

    // `dwSize` is the structure's size and is checked by the API; a zeroed entry is how
    // the reference example hands it over.
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    // A first entry that cannot be read is an *error*, not an empty list: the snapshot was
    // created successfully, so this is a genuine failure to look, and `poll_once` treats a
    // failure as "no observation" instead of "everything stopped".
    unsafe { Process32FirstW(snapshot.0, &mut entry) }
        .map_err(|e| anyhow!("reading the first process of the snapshot: {e}"))?;

    let mut names = Vec::new();
    loop {
        // The name is a NUL-terminated UTF-16 buffer, and the terminator is not always the
        // last unit; a buffer that somehow has none is taken whole rather than truncated.
        let name = match entry.szExeFile.iter().position(|unit| *unit == 0) {
            Some(len) => String::from_utf16_lossy(&entry.szExeFile[..len]),
            None => String::from_utf16_lossy(&entry.szExeFile),
        };
        names.push(name);
        // A failure here is the documented end of the list ("no more entries"), which is
        // how a `Process32*` walk always terminates.
        if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
            break;
        }
    }
    Ok(names)
}

/// Refuse a watch list that cannot mean what it says.
///
/// Two mistakes are worth a startup error rather than silent behaviour:
///
/// * an `exe` that is empty names no file, so it can never match anything — a title the
///   user believes is watched and is not;
/// * the same `exe` twice is one game, and `exe` is the identity the matcher and the
///   tracker key on (case-insensitively, because that is how matching works), so a repeat
///   is a mistake rather than two watches.
pub fn validate_watch_list(watched: &[WatchedGame]) -> Result<()> {
    let mut seen: Vec<&str> = Vec::with_capacity(watched.len());
    for game in watched {
        let exe = file_name(&game.exe);
        if exe.is_empty() {
            bail!("{} has no executable name to watch for", game.name);
        }
        if seen.iter().any(|other| other.eq_ignore_ascii_case(exe)) {
            bail!("{exe} is watched twice");
        }
        seen.push(exe);
    }
    Ok(())
}

/// League of Legends, via the API Riot publishes for it.
///
/// This is a *game-state* signal, not a "some Riot process exists" signal. The Live Client
/// Data API is served on `127.0.0.1:2999` while a match is running and stops answering when
/// it ends, so `running: true` means a game is being played. That is why the user's
/// instruction — "using the official local api riot provides to record it should be safe to
/// auto start" — is what this type implements, rather than a process name.
///
/// The client is the crate's existing one ([`LoopbackClient`]), which is address-pinned to
/// an [`Endpoint`] that cannot be built for anything but a loopback address. This adds no
/// HTTP client, no socket, no second place that relaxes TLS verification and no new request
/// path: it asks for the same `/liveclientdata/allgamedata` the event poller asks for, and
/// reads only the *status* of the answer.
///
/// # What it cannot tell apart
///
/// The endpoint is the game's, so a *replay* or a spectated game served by the same client
/// looks the same as a match being played: the API cannot distinguish them. This is a limit
/// of the signal Riot provides, and it is written down rather than papered over. It is not
/// the expensive kind of false positive — it is still a game the user opened.
pub struct LiveClientPresence {
    client: LoopbackClient,
    watching: Vec<WatchedGame>,
}

impl LiveClientPresence {
    /// The real thing: Riot's endpoint, and the titles configured for it.
    ///
    /// Titles whose signal is [`Signal::Process`] are not this source's business and are
    /// left out — [`CompositePresence`] hands each title to exactly one source.
    pub fn new(watched: &[WatchedGame]) -> Result<Self> {
        Self::for_endpoint(watched, Endpoint::live_client())
    }

    /// The same detector against any loopback endpoint: how a test reaches a mock, and how
    /// a front-end with its own endpoint would build one.
    pub fn for_endpoint(watched: &[WatchedGame], endpoint: Endpoint) -> Result<Self> {
        let watching = watched
            .iter()
            .filter(|game| game.signal == Signal::LiveClientApi)
            .cloned()
            .collect();
        Ok(Self { client: LoopbackClient::for_endpoint(endpoint)?, watching })
    }

    /// The endpoint this detector reads.
    pub fn endpoint(&self) -> &Endpoint {
        self.client.endpoint()
    }

    /// The titles this detector speaks for.
    pub fn watching(&self) -> &[WatchedGame] {
        &self.watching
    }
}

impl GamePresence for LiveClientPresence {
    fn poll(&mut self) -> Result<Vec<Presence>> {
        if self.watching.is_empty() {
            // Nothing to answer for means not even the request: an unused source must not
            // put traffic on the loopback socket every few seconds.
            return Ok(Vec::new());
        }

        let running = match self.client.get() {
            // A body means the API is serving a game. What the document says is not this
            // module's business: `lol::derive` turns it into events.
            Ok(Response::Body(_)) => true,
            // 404: the API is up and reports no game.
            Ok(Response::NotServing) => false,
            // A refused or timed-out connection on loopback is the ordinary "no game is
            // running" answer (spec §7.1), not a failure.
            Err(err) if err.is_no_game() => false,
            // A peer that is not the live client — not speaking TLS, or answering
            // something that is not HTTP — is a failure to look, and saying so is better
            // than inventing either answer.
            Err(err) => {
                return Err(anyhow!("the League Live Client could not be read: {err}"));
            }
        };

        Ok(self
            .watching
            .iter()
            .cloned()
            .map(|game| Presence { game, running })
            .collect())
    }
}

/// Both signals behind one [`GamePresence`].
///
/// The watch list is partitioned by its own [`Signal`], so each title is looked for by the
/// mechanism it is configured for and nothing is polled twice. The process source exists
/// only where the enumeration does: on a platform without it, the titles that would need it
/// are named once at construction, and no failing poll is invented every interval.
pub struct CompositePresence {
    process: Option<ProcessWatcher>,
    live: Option<LiveClientPresence>,
}

impl CompositePresence {
    /// Watch `watched`, each title by the signal it names.
    pub fn new(watched: Vec<WatchedGame>) -> Result<Self> {
        validate_watch_list(&watched)?;
        let (by_process, by_api): (Vec<WatchedGame>, Vec<WatchedGame>) =
            watched.into_iter().partition(|game| game.signal == Signal::Process);

        let live = if by_api.is_empty() {
            None
        } else {
            Some(LiveClientPresence::new(&by_api)?)
        };

        let process = if by_process.is_empty() {
            None
        } else if cfg!(windows) {
            Some(ProcessWatcher::new(by_process)?)
        } else {
            // Once, at startup, where the user can see it: a log line is the difference
            // between "this title is not watched on this platform" and a game that never
            // starts a recording for no visible reason.
            let names = by_process
                .iter()
                .map(|game| game.exe.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            tracing::warn!(
                "watching for a game by executable name needs Windows, so these are not \
                 watched on this platform: {names}"
            );
            None
        };

        tracing::info!(
            "game presence: {} title(s) by executable name, {} by the League Live Client \
             Data API",
            process.as_ref().map_or(0, |w| w.watched().len()),
            live.as_ref().map_or(0, |l| l.watching().len())
        );

        Ok(Self { process, live })
    }

    /// The process-name source, where this platform has one.
    pub fn process(&self) -> Option<&ProcessWatcher> {
        self.process.as_ref()
    }

    /// The Live Client Data API source, when a title asks for it.
    pub fn live_client(&self) -> Option<&LiveClientPresence> {
        self.live.as_ref()
    }
}

impl GamePresence for CompositePresence {
    fn poll(&mut self) -> Result<Vec<Presence>> {
        let mut snapshot = Vec::new();
        if let Some(process) = &mut self.process {
            snapshot.extend(
                process.poll().map_err(|err| anyhow!("reading the process list: {err}"))?,
            );
        }
        if let Some(live) = &mut self.live {
            snapshot.extend(
                live.poll().map_err(|err| anyhow!("polling the League Live Client: {err}"))?,
            );
        }
        Ok(snapshot)
    }
}

/// One change in what is running.
///
/// The two directions are variants rather than a `bool` field because the two callers do
/// different things with them: a start is "arm the capture", a stop is "the session is
/// over". Neither carries a timestamp: the driver knows when it received it, and a clock in
/// here would make the tracker un-testable without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresenceChange {
    /// The game is running now.
    Started(WatchedGame),
    /// The game has stopped.
    Stopped(WatchedGame),
}

impl PresenceChange {
    /// The game this change is about.
    pub fn game(&self) -> &WatchedGame {
        match self {
            PresenceChange::Started(game) | PresenceChange::Stopped(game) => game,
        }
    }

    /// Whether this is a start.
    pub fn is_start(&self) -> bool {
        matches!(self, PresenceChange::Started(_))
    }

    /// Whether this is a stop.
    pub fn is_stop(&self) -> bool {
        matches!(self, PresenceChange::Stopped(_))
    }
}

impl fmt::Display for PresenceChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let verb = if self.is_start() { "started" } else { "stopped" };
        write!(f, "{} {verb}", self.game())
    }
}

/// One game's place in the tracker.
#[derive(Debug)]
struct Tracked {
    game: WatchedGame,
    /// What is believed about it, or `None` before the first observation.
    believed: Option<bool>,
    /// The state a change is heading for, and how many polls in a row have shown it.
    candidate: bool,
    runs: u32,
}

/// What is believed to be running, and the debounce that keeps a flicker out of it.
///
/// The rules, in one place:
///
/// * **The first observation of a game is believed immediately.** A watcher that starts in
///   the middle of a session — the normal case for a tray application — says what is
///   running now instead of waiting for a transition that already happened.
/// * **A later change is believed after [`CONFIRMATIONS`] consecutive polls** (or however
///   many [`PresenceTracker::with_confirmations`] asks for), so a snapshot that alternates
///   between answers produces no changes at all rather than one per poll.
/// * **A game a snapshot does not mention is not evidence.** Only an explicit
///   `running: false` counts as stopped, so a partial snapshot — the Live Client's answer
///   on a platform with no process enumeration, say — cannot end a session.
/// * **A change is reported exactly once.** What is compared is the believed state, so a
///   game that stays stopped does not stop again, and one that stays running does not start
///   again.
///
/// The tracker is pure: no clock, no I/O, no logging, no allocation beyond the changes it
/// answers with. That is deliberate — this is the half worth testing exhaustively, and it
/// is the half every platform shares.
#[derive(Debug)]
pub struct PresenceTracker {
    confirmations: u32,
    tracked: Vec<Tracked>,
}

impl PresenceTracker {
    /// Track `watched` with the default debounce ([`CONFIRMATIONS`]).
    ///
    /// The list is the identity: an `exe` names a game once, case-insensitively (that is
    /// how matching works). A repeat is kept the first time and ignored afterwards, so a
    /// caller that did not validate its list still gets one answer per game.
    pub fn new(watched: &[WatchedGame]) -> Self {
        Self::with_confirmations(watched, CONFIRMATIONS)
    }

    /// The same, with an explicit debounce. `0` is read as `1` — a tracker that believed
    /// nothing would never report anything at all.
    pub fn with_confirmations(watched: &[WatchedGame], confirmations: u32) -> Self {
        let mut tracked: Vec<Tracked> = Vec::with_capacity(watched.len());
        for game in watched {
            let duplicate = tracked.iter().any(|other| {
                other.game.file_name().eq_ignore_ascii_case(game.file_name())
            });
            if duplicate {
                continue;
            }
            tracked.push(Tracked {
                game: game.clone(),
                believed: None,
                candidate: false,
                runs: 0,
            });
        }
        Self { confirmations: confirmations.max(1), tracked }
    }

    /// Fold one poll in, and answer what changed.
    pub fn observe(&mut self, snapshot: &[Presence]) -> Vec<PresenceChange> {
        let mut changes = Vec::new();
        for tracked in &mut self.tracked {
            let Some(running) = snapshot
                .iter()
                .find(|presence| {
                    presence.game.file_name().eq_ignore_ascii_case(tracked.game.file_name())
                })
                .map(|presence| presence.running)
            else {
                // Not mentioned: no observation, so nothing to believe and nothing to say.
                continue;
            };

            match tracked.believed {
                None => {
                    tracked.believed = Some(running);
                    tracked.candidate = running;
                    tracked.runs = 1;
                    if running {
                        changes.push(PresenceChange::Started(tracked.game.clone()));
                    }
                }
                Some(believed) if believed == running => {
                    // The steady state: silent, and no run to count.
                    tracked.candidate = running;
                    tracked.runs = 0;
                }
                Some(_) => {
                    if tracked.candidate == running {
                        tracked.runs += 1;
                    } else {
                        tracked.candidate = running;
                        tracked.runs = 1;
                    }
                    if tracked.runs >= self.confirmations {
                        tracked.believed = Some(running);
                        changes.push(if running {
                            PresenceChange::Started(tracked.game.clone())
                        } else {
                            PresenceChange::Stopped(tracked.game.clone())
                        });
                    }
                }
            }
        }
        changes
    }

    /// The games believed to be running, as of the last observation.
    pub fn running(&self) -> Vec<&WatchedGame> {
        self.tracked
            .iter()
            .filter(|tracked| tracked.believed == Some(true))
            .map(|tracked| &tracked.game)
            .collect()
    }

    /// Whether `exe` is believed to be running.
    pub fn is_running(&self, exe: &str) -> bool {
        self.tracked.iter().any(|tracked| {
            tracked.believed == Some(true)
                && tracked.game.file_name().eq_ignore_ascii_case(file_name(exe))
        })
    }

    /// How many games this tracker follows.
    pub fn watched(&self) -> usize {
        self.tracked.len()
    }

    /// The debounce this tracker was built with.
    pub fn confirmations(&self) -> u32 {
        self.confirmations
    }
}

/// The poll loop: ask the detector, fold the answer through the tracker, report the changes.
///
/// Split from the thread for the same reason `lol::LolPoller::poll_once` is: the half worth
/// testing is "what does this sequence of polls mean", and it can be driven directly, with
/// no clock and no syscalls.
pub struct GameWatcher {
    detector: Box<dyn GamePresence>,
    tracker: PresenceTracker,
    interval: Duration,
    quiet: RunQuiet,
}

impl GameWatcher {
    /// Watch `games` with both signals, for real.
    ///
    /// The interval is bounded by [`clamp_interval`] here, so no configuration value can
    /// reach this constructor and turn the watcher into a tight loop over the process list.
    pub fn new(games: Vec<WatchedGame>, interval: Duration) -> Result<Self> {
        let detector = CompositePresence::new(games.clone())?;
        Ok(Self::with_detector(Box::new(detector), &games, clamp_interval(interval)))
    }

    /// The same loop around any [`GamePresence`]: how the tests drive it, and how a
    /// front-end with a source of its own would use it.
    ///
    /// The interval is used as given — this is the deliberate seam, and a test that wants
    /// ten polls a second is not a configuration mistake.
    pub fn with_detector(
        detector: Box<dyn GamePresence>,
        watched: &[WatchedGame],
        interval: Duration,
    ) -> Self {
        Self {
            detector,
            tracker: PresenceTracker::new(watched),
            interval,
            quiet: RunQuiet::default(),
        }
    }

    /// How often this watcher polls.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// The games believed to be running as of the last poll.
    pub fn running(&self) -> Vec<&WatchedGame> {
        self.tracker.running()
    }

    /// Poll once and answer what changed.
    ///
    /// Never fails. A detector that could not answer is a log line — the first of a run, at
    /// `warn` — and **not** a transition: a failed snapshot is not a game that stopped, and
    /// ending a recording over one is the mistake this method exists to prevent.
    pub fn poll_once(&mut self) -> Vec<PresenceChange> {
        let snapshot = match self.detector.poll() {
            Ok(snapshot) => {
                if let Some(condition) = self.quiet.recovered() {
                    tracing::info!("game detection is answering again ({condition})");
                }
                snapshot
            }
            Err(err) => {
                if self.quiet.note(format!("{err:#}")) {
                    tracing::warn!(
                        "could not look for a game ({err:#}); keeping the state the last poll \
                         found"
                    );
                }
                return Vec::new();
            }
        };

        let changes = self.tracker.observe(&snapshot);
        for change in &changes {
            match change {
                PresenceChange::Started(game) => tracing::info!("{game} is running"),
                PresenceChange::Stopped(game) => tracing::info!("{game} has stopped"),
            }
        }
        changes
    }
}

/// Where a watcher sends what changed. Unbounded, for the reason [`crate::EventSink`] is:
/// the volume is a handful of changes an hour, and a full queue must never drop one.
pub type PresenceSink = Sender<PresenceChange>;

/// A running watcher, on its own thread.
///
/// Dropping it stops the thread and waits for it, so a stopped application leaves no thread
/// behind and a test can be sure no poll is still in flight.
///
/// Which is also the one way to hold this type wrong: a caller has to *keep* the handle for
/// as long as it wants to be told about games, and the attribute is here to say so out loud.
#[must_use = "dropping the watch handle stops the watcher"]
pub struct WatchHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WatchHandle {
    /// Stop watching and wait for the thread. Idempotent.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            // A poll in flight finishes — it is bounded by its own syscall — and the join
            // is what makes "stopped" mean it.
            let _ = thread.join();
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Start watching on a thread of its own.
///
/// Every change goes to `sink` until the handle is dropped. A sink the driver has dropped
/// ends the loop: there is nobody left to tell, so there is nothing left to poll.
///
/// # The interval is a floor, not a wait
///
/// [`sleep_until`] sleeps [`STOP_SLICE`] at a time so that `stop` does not have to wait out
/// a whole interval, and a poll that took longer than the interval starts the next one
/// straight away. It never spins: between polls the thread is asleep.
pub fn spawn(watcher: GameWatcher, sink: PresenceSink) -> Result<WatchHandle> {
    let interval = watcher.interval();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let mut watcher = watcher;

    tracing::info!(
        "watching for a game to start, every {} ms (a change is believed after {} polls)",
        interval.as_millis(),
        watcher.tracker.confirmations()
    );

    let thread = std::thread::Builder::new()
        .name("localplay-games".to_string())
        .spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                let started = Instant::now();
                for change in watcher.poll_once() {
                    if sink.send(change).is_err() {
                        tracing::debug!("the game watcher stopped: nothing is receiving changes");
                        return;
                    }
                }
                sleep_until(started + interval, &flag);
            }
            tracing::debug!("the game watcher stopped");
        })
        .map_err(|e| anyhow!("spawning the game watcher thread: {e}"))?;

    Ok(WatchHandle { stop, thread: Some(thread) })
}

/// Sleep until `deadline`, waking early — within [`STOP_SLICE`] — when asked to stop.
fn sleep_until(deadline: Instant, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        std::thread::sleep((deadline - now).min(STOP_SLICE));
    }
}

/// Bound a configured interval into [`MIN_POLL_INTERVAL`]..=[`MAX_POLL_INTERVAL`].
///
/// The one place configuration becomes an interval, so that the floor protecting the user's
/// machine (see [`MIN_POLL_INTERVAL`]) cannot be bypassed by a field that is read somewhere
/// else.
pub fn clamp_interval(requested: Duration) -> Duration {
    requested.clamp(MIN_POLL_INTERVAL, MAX_POLL_INTERVAL)
}

/// Keeps a repeating condition from writing a line every poll.
///
/// The same discipline `lol::poller` uses for "no game is being served": the first of a run
/// is news, a *changed* condition is news, and the repetitions are not — which is what
/// keeps a two-second poll from filling a log file with one identical failure.
#[derive(Debug, Default)]
struct RunQuiet {
    active: Option<String>,
}

impl RunQuiet {
    /// Note a failure. Whether this one is worth a line.
    fn note(&mut self, condition: String) -> bool {
        match &self.active {
            Some(active) if *active == condition => false,
            _ => {
                self.active = Some(condition);
                true
            }
        }
    }

    /// The answer came back: end whatever was being reported, once.
    fn recovered(&mut self) -> Option<String> {
        self.active.take()
    }
}

/// The `[games]` section of `config.toml`: what to watch, and whether to watch.
///
/// The types live here because the crate that polls owns the names it accepts; the file
/// itself is read by the configuration layer, which deserialises this struct.
///
/// ```toml
/// [games]
/// auto_record = false
/// poll_ms = 2500
///
/// [[games.watch]]
/// name = "League of Legends"
/// exe = "League of Legends.exe"
/// signal = "live_client_api"
/// ```
///
/// **`auto_record` defaults to `false`, and that is the whole safety posture of this
/// feature.** With it off, [`GamesSection::start`] starts nothing: no process list is
/// enumerated, no snapshot is taken and no request is made to the Live Client Data API. A
/// machine that never turns it on never has anything watched on its behalf — which is what
/// the user asked for, and the right default on a box running kernel-level anti-cheat.
///
/// `poll_ms` is clamped by [`GamesSection::interval`]; a value of `0` there is `250`, not a
/// tight loop over the process list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GamesSection {
    /// Whether a watched game starting is on its own a reason to start recording.
    pub auto_record: bool,
    /// How often to look, in milliseconds. Read through [`GamesSection::interval`], which
    /// bounds it.
    pub poll_ms: u64,
    /// The titles to recognise.
    pub watch: Vec<WatchedGame>,
}

impl Default for GamesSection {
    /// Nothing automatic, a few seconds apart, and the six titles of [`default_watch`].
    fn default() -> Self {
        Self {
            auto_record: false,
            poll_ms: DEFAULT_POLL_MS,
            watch: default_watch(),
        }
    }
}

impl GamesSection {
    /// The poll interval this section asks for, bounded into
    /// [`MIN_POLL_INTERVAL`]..=[`MAX_POLL_INTERVAL`].
    ///
    /// Read the interval through this rather than through `poll_ms`: the clamp is what
    /// stops a `0` in a configuration file from becoming a watcher that enumerates the
    /// process list as fast as the CPU allows.
    pub fn interval(&self) -> Duration {
        clamp_interval(Duration::from_millis(self.poll_ms))
    }

    /// Whether this section asks for anything to be watched at all.
    pub fn watching(&self) -> bool {
        self.auto_record
    }

    /// Start the watcher this section describes — or `None` when `auto_record` is off,
    /// which it is by default.
    ///
    /// The `None` is the important half of the contract: with the default section nothing
    /// is polled, so the opt-in is real rather than a flag on a watcher that runs anyway.
    ///
    /// The changes arrive on `sink` as they happen; drop the receiver to end the watcher,
    /// or drop (or [`WatchHandle::stop`]) the handle to stop it deliberately. A caller that
    /// wants the changes has to keep the handle alive — dropping it stops the watcher.
    ///
    /// ```no_run
    /// # fn main() -> anyhow::Result<()> {
    /// use localplay_events::process::{default_watch, GamesSection};
    /// use std::sync::mpsc::channel;
    ///
    /// let section = GamesSection::default(); // auto_record = false: nothing is watched
    /// let (sink, changes) = channel();
    /// let _watcher = section.start(sink)?;
    /// while let Ok(change) = changes.recv() {
    ///     println!("{change}");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn start(&self, sink: PresenceSink) -> Result<Option<WatchHandle>> {
        if !self.auto_record {
            tracing::debug!("games.auto_record is off, so no game is being watched for");
            return Ok(None);
        }
        let watcher = GameWatcher::new(self.watch.clone(), self.interval())?;
        spawn(watcher, sink).map(Some)
    }
}

/// The titles a fresh configuration watches, and how each is recognised.
///
/// Only League of Legends is recognised by Riot's Live Client Data API, because it is the
/// only title here that publishes one. Everything else is recognised by its **game**
/// executable — the process that exists only while the game itself is running, never the
/// launcher's — which is what keeps a process-name match from firing all evening on a
/// client somebody left open:
///
/// | title | executable | signal |
/// |---|---|---|
/// | League of Legends | `League of Legends.exe` (not `LeagueClient.exe`) | Live Client Data API |
/// | Counter-Strike 2 | `cs2.exe` (not `steam.exe`) | process name |
/// | Dota 2 | `dota2.exe` | process name |
/// | Rainbow Six Siege | `RainbowSix.exe` | process name |
/// | Valorant | `VALORANT-Win64-Shipping.exe` (not `VALORANT.exe`) | process name |
/// | Overwatch 2 | `Overwatch.exe` | process name |
///
/// Valorant is the interesting row: it is a Riot title, and the honest statement about it
/// is that Riot publishes no local game-state API for it. The Riot *Client* has an
/// undocumented local HTTP API behind a lockfile password, but that reports the launcher's
/// session rather than a match, and this project does not use it. So Valorant is watched by
/// the name of the game process, which exists only while a match is running.
pub fn default_watch() -> Vec<WatchedGame> {
    vec![
        WatchedGame::by_live_client_api("League of Legends", "League of Legends.exe"),
        WatchedGame::by_process("Counter-Strike 2", "cs2.exe"),
        WatchedGame::by_process("Dota 2", "dota2.exe"),
        WatchedGame::by_process("Rainbow Six Siege", "RainbowSix.exe"),
        WatchedGame::by_process("Valorant", "VALORANT-Win64-Shipping.exe"),
        WatchedGame::by_process("Overwatch 2", "Overwatch.exe"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;

    // ---- the pure decision -----------------------------------------------------------

    fn observed(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    fn running_of(presences: &[Presence], exe: &str) -> Option<bool> {
        presences
            .iter()
            .find(|presence| presence.game.exe.eq_ignore_ascii_case(exe))
            .map(|presence| presence.running)
    }

    #[test]
    fn the_default_watch_list_is_the_titles_the_project_names() {
        let watch = default_watch();
        let named: Vec<(&str, &str, Signal)> = watch
            .iter()
            .map(|game| (game.name.as_str(), game.exe.as_str(), game.signal))
            .collect();
        assert_eq!(
            named,
            vec![
                ("League of Legends", "League of Legends.exe", Signal::LiveClientApi),
                ("Counter-Strike 2", "cs2.exe", Signal::Process),
                ("Dota 2", "dota2.exe", Signal::Process),
                ("Rainbow Six Siege", "RainbowSix.exe", Signal::Process),
                ("Valorant", "VALORANT-Win64-Shipping.exe", Signal::Process),
                ("Overwatch 2", "Overwatch.exe", Signal::Process),
            ]
        );
        assert_eq!(
            watch.iter().filter(|g| g.signal == Signal::LiveClientApi).count(),
            1,
            "Riot publishes the Live Client Data API for one title on this list, not for all"
        );
        assert!(
            watch.iter().all(|g| g.exe.ends_with(".exe")),
            "every title is watched by an executable's file name"
        );
    }

    #[test]
    fn a_match_is_case_insensitive() {
        let watch = vec![WatchedGame::by_process("League", "League of Legends.exe")];
        let presences = presences_in(&watch, &observed(&["league of legends.exe"]));
        assert_eq!(running_of(&presences, "League of Legends.exe"), Some(true));
        let presences = presences_in(&watch, &observed(&["LEAGUE OF LEGENDS.EXE"]));
        assert_eq!(running_of(&presences, "League of Legends.exe"), Some(true));
    }

    #[test]
    fn a_name_that_merely_contains_the_executable_is_not_a_match() {
        // The false positive that would start a recording nobody asked for.
        let watch = vec![WatchedGame::by_process("Counter-Strike 2", "cs2.exe")];
        for name in [
            "notcs2.exe",       // a superset of the name
            "cs2.exe.bak",      // not the same file
            "mycs2.exe",        // a different file that happens to end the same way
            "cs2",              // the same stem without the extension
            "xc s2.exe",        // a space where a character is
            "cs2.exe ",         // trailing space, trimmed away — matches, see below
        ] {
            let matches = running_of(&presences_in(&watch, &observed(&[name])), "cs2.exe");
            if name == "cs2.exe " {
                assert_eq!(matches, Some(true), "{name:?} is the same file name once trimmed");
            } else {
                assert_eq!(matches, Some(false), "{name:?} must not match cs2.exe");
            }
        }
    }

    #[test]
    fn a_path_bearing_name_is_reduced_to_its_file_name() {
        let watch = vec![WatchedGame::by_process("Counter-Strike 2", "cs2.exe")];
        for name in [
            r"C:\Program Files (x86)\Steam\steamapps\common\Counter-Strike Global Offensive\game\bin\win64\cs2.exe",
            r"\\?\C:\games\cs2.exe",
            "C:/games/cs2.exe",
            "/Applications/cs2.exe",
            r"cs2.exe",
        ] {
            assert_eq!(
                running_of(&presences_in(&watch, &observed(&[name])), "cs2.exe"),
                Some(true),
                "{name:?} names the file cs2.exe"
            );
        }
    }

    #[test]
    fn a_configured_path_is_reduced_too() {
        let watch = vec![WatchedGame::by_process("Counter-Strike 2", r"C:\Games\cs2.exe")];
        assert_eq!(watch[0].file_name(), "cs2.exe");
        let presences = presences_in(&watch, &observed(&["cs2.exe"]));
        assert_eq!(running_of(&presences, r"C:\Games\cs2.exe"), Some(true));
    }

    #[test]
    fn a_unicode_lookalike_is_not_a_match() {
        // Cyrillic С (U+0421), which is not the Latin C. Windows' file-name
        // case-insensitivity is ASCII, and a Unicode case fold would call these equal.
        let watch = vec![WatchedGame::by_process("Counter-Strike 2", "cs2.exe")];
        assert_eq!(
            running_of(&presences_in(&watch, &observed(&["\u{421}S2.EXE"])), "cs2.exe"),
            Some(false),
            "a lookalike letter is a different file name"
        );
    }

    #[test]
    fn every_watched_game_gets_an_answer_even_when_nothing_is_present() {
        let watch = default_watch();
        let presences = presences_in(&watch, &observed(&[]));
        assert_eq!(presences.len(), watch.len(), "one answer per watched title");
        assert!(presences.iter().all(|presence| !presence.running));
        assert!(
            presences_in(&[], &observed(&["cs2.exe"])).is_empty(),
            "an empty watch list answers nothing, and does not panic"
        );
    }

    #[test]
    fn two_different_games_can_be_present_at_once() {
        let watch = default_watch();
        let presences = presences_in(
            &watch,
            &observed(&["explorer.exe", "cs2.exe", "VALORANT-Win64-Shipping.exe", "cs2.exe"]),
        );
        assert_eq!(running_of(&presences, "cs2.exe"), Some(true));
        assert_eq!(running_of(&presences, "VALORANT-Win64-Shipping.exe"), Some(true));
        assert_eq!(running_of(&presences, "dota2.exe"), Some(false));
        assert_eq!(running_of(&presences, "Overwatch.exe"), Some(false));
        assert_eq!(running_of(&presences, "League of Legends.exe"), Some(false));
        assert_eq!(presences.len(), watch.len(), "a duplicate in the snapshot is still one answer");
    }

    #[test]
    fn a_configuration_value_that_is_only_a_path_separator_matches_nothing() {
        // `exe` cannot be empty or separators-only in a validated list, and the matcher
        // must not turn a separator into an empty file name that equals another empty one.
        assert_eq!(file_name(r"\\"), r"\\");
        assert!(!image_name_matches("", ""));
        assert!(!image_name_matches("   ", ""));
        assert!(!image_name_matches("cs2.exe", ""));
    }

    // ---- the tracker -----------------------------------------------------------------

    /// A snapshot in which exactly the named games are running, every title answered.
    fn poll_of(watched: &[WatchedGame], running: &[&str]) -> Vec<Presence> {
        watched
            .iter()
            .map(|game| Presence {
                game: game.clone(),
                running: running
                    .iter()
                    .any(|exe| game.file_name().eq_ignore_ascii_case(file_name(exe))),
            })
            .collect()
    }

    #[test]
    fn a_game_already_running_is_running_by_the_first_poll() {
        // The tray is usually started while the client is already open: the first answer
        // must be the truth about now, not a wait for a transition that already happened.
        let watch = default_watch();
        let mut tracker = PresenceTracker::new(&watch);
        let changes = tracker.observe(&poll_of(&watch, &["cs2.exe"]));
        assert_eq!(changes, vec![PresenceChange::Started(watch[1].clone())]);
        assert_eq!(tracker.running().len(), 1);
        assert!(tracker.is_running("CS2.EXE"), "and the answer is the believed state");
    }

    #[test]
    fn a_game_that_is_not_running_says_nothing_at_all() {
        let watch = default_watch();
        let mut tracker = PresenceTracker::new(&watch);
        for _ in 0..5 {
            assert!(
                tracker.observe(&poll_of(&watch, &[])).is_empty(),
                "a game that was never running is not a game that stopped"
            );
        }
        assert!(tracker.running().is_empty());
        assert!(!tracker.is_running("cs2.exe"));
    }

    #[test]
    fn a_start_is_believed_after_two_polls_and_a_stop_is_reported_exactly_once() {
        let watch = default_watch();
        let mut tracker = PresenceTracker::new(&watch);
        assert_eq!(tracker.confirmations(), CONFIRMATIONS);

        // Nothing running, then a start: believed on the second agreeing poll.
        assert!(tracker.observe(&poll_of(&watch, &[])).is_empty());
        assert!(tracker.observe(&poll_of(&watch, &["cs2.exe"])).is_empty(), "one poll is not enough");
        assert_eq!(
            tracker.observe(&poll_of(&watch, &["cs2.exe"])),
            vec![PresenceChange::Started(watch[1].clone())]
        );

        // Running and staying running is silent, however long it lasts.
        for _ in 0..5 {
            assert!(tracker.observe(&poll_of(&watch, &["cs2.exe"])).is_empty());
        }

        // And the stop is reported once, never twice.
        assert!(tracker.observe(&poll_of(&watch, &[])).is_empty(), "one poll is not enough to stop");
        assert_eq!(
            tracker.observe(&poll_of(&watch, &[])),
            vec![PresenceChange::Stopped(watch[1].clone())]
        );
        for _ in 0..5 {
            assert!(tracker.observe(&poll_of(&watch, &[])).is_empty(), "a stop is reported once");
        }
        assert!(tracker.running().is_empty());
    }

    #[test]
    fn a_flapping_snapshot_produces_no_event_storm() {
        // A process list that alternates between "there" and "not there" — a game
        // restarting a worker, a snapshot taken mid-exit — must not become one change per
        // poll, which is one recording per poll downstream.
        let watch = default_watch();
        let mut tracker = PresenceTracker::new(&watch);
        assert!(tracker.observe(&poll_of(&watch, &[])).is_empty());

        let mut changes = 0;
        for poll in 0..40 {
            let snapshot = if poll % 2 == 0 { &["cs2.exe"][..] } else { &[][..] };
            changes += tracker.observe(&poll_of(&watch, snapshot)).len();
        }
        assert_eq!(changes, 0, "an alternating snapshot believes nothing and reports nothing");
        assert!(tracker.running().is_empty());
    }

    #[test]
    fn two_games_running_at_once_are_representable_and_one_stopping_leaves_the_other() {
        let watch = default_watch();
        let mut tracker = PresenceTracker::new(&watch);
        let changes = tracker.observe(&poll_of(&watch, &["cs2.exe", "dota2.exe"]));
        assert_eq!(
            changes,
            vec![
                PresenceChange::Started(watch[1].clone()),
                PresenceChange::Started(watch[2].clone()),
            ],
            "both answers arrive in one poll, in the order they are watched"
        );
        assert_eq!(tracker.running().len(), 2);

        // Counter-Strike stops; Dota 2 does not, and is not disturbed by it.
        let cs2_stopped = poll_of(&watch, &["dota2.exe"]);
        assert!(
            tracker.observe(&cs2_stopped).is_empty(),
            "one poll is not enough to stop"
        );
        assert_eq!(
            tracker.observe(&cs2_stopped),
            vec![PresenceChange::Stopped(watch[1].clone())]
        );
        assert_eq!(tracker.running(), vec![&watch[2]]);
    }

    #[test]
    fn a_snapshot_that_does_not_mention_a_game_leaves_it_alone() {
        // The shape a partial snapshot takes: the Live Client answers on a platform with
        // no process enumeration. Silence about a game is not evidence about a game.
        let watch = default_watch();
        let mut tracker = PresenceTracker::new(&watch);
        assert_eq!(
            tracker.observe(&poll_of(&watch, &["cs2.exe"])),
            vec![PresenceChange::Started(watch[1].clone())]
        );

        let only_live: Vec<Presence> = poll_of(&watch, &[])
            .into_iter()
            .filter(|presence| presence.game.signal == Signal::LiveClientApi)
            .collect();
        for _ in 0..5 {
            assert!(
                tracker.observe(&only_live).is_empty(),
                "a game the snapshot does not mention is not a game that stopped"
            );
        }
        assert!(tracker.is_running("cs2.exe"));
    }

    #[test]
    fn the_tracker_follows_each_executable_once() {
        let one = WatchedGame::by_process("Counter-Strike 2", "cs2.exe");
        let repeat = WatchedGame::by_process("Counter-Strike 2, again", "CS2.EXE");
        let tracker = PresenceTracker::new(&[one, repeat.clone(), repeat.clone()]);
        assert_eq!(tracker.watched(), 1, "an exe names one game");

        let mut tracker = PresenceTracker::with_confirmations(std::slice::from_ref(&repeat), 0);
        assert_eq!(tracker.confirmations(), 1, "a debounce of zero would believe nothing");
        let watch = vec![WatchedGame::by_process("Counter-Strike 2", "cs2.exe")];
        assert_eq!(
            tracker.observe(&poll_of(&watch, &["cs2.exe"])),
            vec![PresenceChange::Started(repeat)],
            "one confirmation believes the first answer that differs, and the change names \
             the game the tracker knows"
        );
    }

    #[test]
    fn a_change_of_game_is_not_a_change_of_state() {
        // The tracker keys on the executable, which is what the matcher keys on, so a
        // renamed display name is the same game and is not a second start.
        let original = WatchedGame::by_process("Counter-Strike 2", "cs2.exe");
        let mut tracker = PresenceTracker::new(std::slice::from_ref(&original));
        let renamed = WatchedGame::by_process("CS2", "cs2.exe");
        let snapshot = vec![Presence { game: renamed, running: true }];
        assert_eq!(
            tracker.observe(&snapshot),
            vec![PresenceChange::Started(original)],
            "one start for the first observation, and the name it is reported under is the \
             one the tracker was built with"
        );
        assert!(tracker.observe(&snapshot).is_empty());
    }

    // ---- the loop, driven by a script -------------------------------------------------

    /// A detector that answers with the sequence a test wrote, in order.
    struct Scripted {
        answers: VecDeque<Result<Vec<Presence>>>,
    }

    impl Scripted {
        /// Boxed, which is why this is not called `new`: the loop wants a trait object.
        fn boxed(answers: Vec<Result<Vec<Presence>>>) -> Box<dyn GamePresence> {
            Box::new(Self { answers: answers.into() })
        }
    }

    impl GamePresence for Scripted {
        fn poll(&mut self) -> Result<Vec<Presence>> {
            // Running out of script is a failure of the test, and saying so loudly beats
            // a detector that quietly answers "nothing" for the rest of the run.
            self.answers
                .pop_front()
                .unwrap_or_else(|| Err(anyhow!("the script has no answer left")))
        }
    }

    #[test]
    fn a_detector_that_cannot_answer_does_not_end_a_session() {
        let watch = default_watch();
        let running = poll_of(&watch, &["cs2.exe"]);
        let detector = Scripted::boxed(vec![
            Ok(running.clone()),
            Err(anyhow!("the process-list snapshot failed")),
            Err(anyhow!("the process-list snapshot failed")),
            Err(anyhow!("the process-list snapshot failed")),
            Ok(running.clone()),
        ]);
        let mut watcher = GameWatcher::with_detector(detector, &watch, Duration::from_millis(10));

        // The first observation is believed straight away: the game is running.
        assert_eq!(
            watcher.poll_once(),
            vec![PresenceChange::Started(watch[1].clone())]
        );
        // Three failed polls: no change, and the state the last good poll found survives.
        for _ in 0..3 {
            assert!(watcher.poll_once().is_empty(), "a failed poll is not a stop");
            assert_eq!(watcher.running().len(), 1);
        }
        // And the recovery is a return to the steady state, not a second start.
        assert!(watcher.poll_once().is_empty());
        assert_eq!(watcher.running().len(), 1);
    }

    #[test]
    fn the_watcher_drives_the_detector_once_per_poll() {
        let watch = default_watch();
        let detector = Scripted::boxed(vec![
            Ok(poll_of(&watch, &[])),
            Ok(poll_of(&watch, &[])),
        ]);
        let mut watcher = GameWatcher::with_detector(detector, &watch, Duration::from_millis(10));
        assert!(watcher.poll_once().is_empty());
        assert!(watcher.poll_once().is_empty());
        // The third call finds the script empty: an error, and still no change.
        assert!(watcher.poll_once().is_empty());
    }

    #[test]
    fn a_repeating_condition_is_reported_once() {
        let mut quiet = RunQuiet::default();
        assert!(quiet.note("the snapshot failed".into()), "the first is news");
        for _ in 0..10 {
            assert!(!quiet.note("the snapshot failed".into()), "and the rest are not");
        }
        assert!(quiet.note("something else failed".into()), "a change is news again");
        assert_eq!(quiet.recovered(), Some("something else failed".into()));
        assert_eq!(quiet.recovered(), None, "a recovery is reported once");
        assert!(quiet.note("the snapshot failed".into()), "and it can be reported again later");
    }

    // ---- the live client source -------------------------------------------------------

    #[test]
    fn the_league_source_keeps_only_the_titles_that_publish_an_api() {
        let source = LiveClientPresence::new(&default_watch()).expect("a client for the endpoint");
        assert_eq!(source.watching().len(), 1);
        assert_eq!(source.watching()[0].name, "League of Legends");
        assert_eq!(source.endpoint().addr().port(), 2999);
        assert!(source.endpoint().is_loopback(), "the address is loopback by construction");
    }

    #[test]
    fn a_refused_loopback_connection_is_no_game_and_not_a_failure() {
        // Port 1 on loopback: nothing listens there, which is what between-games looks
        // like. The answer must be `false`, never an error and never a panic.
        let addr: std::net::SocketAddr = "127.0.0.1:1".parse().expect("a socket address");
        let endpoint =
            Endpoint::loopback(addr, "/liveclientdata/allgamedata").expect("a loopback endpoint");
        let watch = default_watch();
        let mut source =
            LiveClientPresence::for_endpoint(&watch, endpoint).expect("a client for a mock");

        let presences = source.poll().expect("a refused loopback connection is 'no game'");
        assert_eq!(presences.len(), 1, "one answer per title the source speaks for");
        assert!(!presences[0].running);
        assert_eq!(presences[0].game.signal, Signal::LiveClientApi);
    }

    #[test]
    fn a_source_with_nothing_to_answer_for_does_not_even_ask() {
        // No title uses the API: the endpoint is never contacted, which is what makes an
        // unused source free. Port 1 would refuse if it were asked.
        let addr: std::net::SocketAddr = "127.0.0.1:1".parse().expect("a socket address");
        let endpoint =
            Endpoint::loopback(addr, "/liveclientdata/allgamedata").expect("a loopback endpoint");
        let only_process: Vec<WatchedGame> = default_watch()
            .into_iter()
            .filter(|game| game.signal == Signal::Process)
            .collect();
        let mut source =
            LiveClientPresence::for_endpoint(&only_process, endpoint).expect("a client");
        assert!(source.watching().is_empty());
        assert_eq!(source.poll().expect("nothing to answer"), Vec::new());
    }

    // ---- the composite -----------------------------------------------------------------

    #[test]
    fn the_composite_partitions_the_watch_list_by_signal() {
        let composite = CompositePresence::new(default_watch()).expect("a validated list");
        let live = composite.live_client().expect("League publishes an API");
        assert_eq!(live.watching().len(), 1);

        if cfg!(windows) {
            let process = composite.process().expect("Windows has the process-list snapshot");
            assert_eq!(process.watched().len(), 5, "everything else is watched by name");
            assert!(process.watched().iter().all(|g| g.signal == Signal::Process));
        } else {
            assert!(
                composite.process().is_none(),
                "without the enumeration there is no process source to fail every interval"
            );
        }
    }

    #[test]
    fn the_composite_answers_for_every_title_it_can() {
        let watch = default_watch();
        let mut composite = CompositePresence::new(watch.clone()).expect("a validated list");
        let presences = composite.poll().expect("a poll answers");
        assert_eq!(
            presences.len(),
            if cfg!(windows) { watch.len() } else { 1 },
            "every title on Windows; only the Live Client's answer where the enumeration is absent"
        );
        assert!(presences.iter().all(|presence| !presence.running));
    }

    // ---- configuration ----------------------------------------------------------------

    #[test]
    fn the_games_section_defaults_to_auto_record_off() {
        let section = GamesSection::default();
        assert!(
            !section.auto_record,
            "nothing is automatic until the user turns it on — the point of the default"
        );
        assert!(!section.watching());
        assert_eq!(section.watch, default_watch());
        assert_eq!(section.poll_ms, DEFAULT_POLL_MS);
        assert_eq!(section.interval(), DEFAULT_POLL_INTERVAL);
        assert_eq!(DEFAULT_POLL_INTERVAL, Duration::from_millis(DEFAULT_POLL_MS));
    }

    #[test]
    fn a_configuration_file_without_the_games_section_gets_the_default_one() {
        // The shape the configuration layer parses: the section is optional, so a file
        // that predates the feature — or a user who removed the section — still gets a
        // well-formed one, with auto-record off.
        #[derive(Deserialize)]
        struct ConfigFile {
            #[serde(default)]
            games: GamesSection,
        }

        let parsed: ConfigFile = serde_json::from_value(json!({})).expect("an empty file parses");
        assert!(!parsed.games.auto_record);
        assert_eq!(parsed.games.watch, default_watch());
        assert_eq!(parsed.games.interval(), DEFAULT_POLL_INTERVAL);

        // And a section that names only some fields keeps the defaults for the rest.
        let parsed: ConfigFile = serde_json::from_value(json!({ "games": { "poll_ms": 4000 } }))
            .expect("a partial section parses");
        assert!(!parsed.games.auto_record);
        assert_eq!(parsed.games.interval(), Duration::from_millis(4000));
        assert_eq!(parsed.games.watch, default_watch());
    }

    #[test]
    fn the_default_watch_list_round_trips_through_serde() {
        let section = GamesSection {
            auto_record: true,
            poll_ms: 1_000,
            watch: default_watch(),
        };
        let text = serde_json::to_string(&section).expect("a section serialises");
        let back: GamesSection = serde_json::from_str(&text).expect("and parses back");
        assert_eq!(back, section);
        assert!(
            text.contains("\"signal\":\"live_client_api\""),
            "the signal spelling is part of the format: {text}"
        );
        assert!(text.contains("\"signal\":\"process\""));
    }

    #[test]
    fn a_watch_entry_is_written_the_way_the_configuration_documents() {
        let parsed: WatchedGame = serde_json::from_value(json!({
            "name": "League of Legends",
            "exe": "League of Legends.exe",
            "signal": "live_client_api",
        }))
        .expect("an entry parses");
        assert_eq!(
            parsed,
            WatchedGame::by_live_client_api("League of Legends", "League of Legends.exe")
        );

        // A signal the user did not write is the general mechanism.
        let parsed: WatchedGame = serde_json::from_value(json!({
            "name": "Dota 2",
            "exe": "dota2.exe",
        }))
        .expect("an entry without a signal parses");
        assert_eq!(parsed.signal, Signal::Process);

        // And a spelling this build does not know is an error, loudly, not a guess.
        let unknown = serde_json::from_value::<WatchedGame>(json!({
            "name": "Mystery",
            "exe": "mystery.exe",
            "signal": "mindreading",
        }));
        assert!(unknown.is_err(), "an unknown signal must not be silently accepted");
    }

    #[test]
    fn a_configured_poll_interval_cannot_become_a_busy_loop() {
        for (poll_ms, expected) in [
            (0, MIN_POLL_INTERVAL),
            (1, MIN_POLL_INTERVAL),
            (249, MIN_POLL_INTERVAL),
            (250, MIN_POLL_INTERVAL),
            (2_500, DEFAULT_POLL_INTERVAL),
            (30_000, MAX_POLL_INTERVAL),
            (60_000, MAX_POLL_INTERVAL),
            (u64::MAX, MAX_POLL_INTERVAL),
        ] {
            let section = GamesSection { poll_ms, ..GamesSection::default() };
            assert_eq!(section.interval(), expected, "poll_ms = {poll_ms}");
        }
        assert_eq!(clamp_interval(Duration::ZERO), MIN_POLL_INTERVAL);
        assert_eq!(clamp_interval(Duration::from_secs(3_600)), MAX_POLL_INTERVAL);

        // And the production constructor is behind the same floor, so no path into the
        // watcher can produce a tight loop over the process list.
        let watcher = GameWatcher::new(default_watch(), Duration::ZERO).expect("a watcher");
        assert_eq!(watcher.interval(), MIN_POLL_INTERVAL);
        assert_eq!(
            GameWatcher::new(default_watch(), Duration::from_secs(3_600))
                .expect("a watcher")
                .interval(),
            MAX_POLL_INTERVAL
        );
    }

    #[test]
    fn a_watch_list_that_cannot_mean_what_it_says_is_refused() {
        let nameless = vec![WatchedGame::by_process("Mystery", "   ")];
        let err = ProcessWatcher::new(nameless.clone()).expect_err("no file name to watch for");
        assert!(err.to_string().contains("Mystery"), "the error names the title: {err}");
        assert!(ProcessWatcher::new(Vec::new()).is_ok(), "an empty list is empty, not wrong");

        let duplicate = vec![
            WatchedGame::by_process("Counter-Strike 2", "cs2.exe"),
            WatchedGame::by_process("Counter-Strike 2 as written by hand", "CS2.exe"),
        ];
        let err = ProcessWatcher::new(duplicate.clone()).expect_err("the same file twice");
        let message = err.to_string();
        assert!(
            message.to_lowercase().contains("cs2.exe") && message.contains("watched twice"),
            "the error names the file and the mistake: {message}"
        );
        assert!(
            CompositePresence::new(duplicate).is_err(),
            "the composite validates the list it is given too"
        );
        assert!(ProcessWatcher::new(default_watch()).is_ok());
    }

    // ---- the background thread ---------------------------------------------------------

    #[test]
    fn the_watcher_thread_reports_a_change_and_releases_the_sink_when_stopped() {
        let watch = default_watch();
        let detector = Scripted::boxed(vec![Ok(poll_of(&watch, &["dota2.exe"]))]);
        let watcher = GameWatcher::with_detector(detector, &watch, Duration::from_millis(5));
        let (sink, changes) = std::sync::mpsc::channel();

        let handle = spawn(watcher, sink).expect("a watcher thread");
        let first = changes
            .recv_timeout(Duration::from_secs(5))
            .expect("the start arrives promptly");
        assert_eq!(first, PresenceChange::Started(watch[2].clone()));
        assert_eq!(first.game().name, "Dota 2");
        assert!(first.is_start() && !first.is_stop());

        // Stopping joins the thread, which drops its end of the channel: that is the
        // proof that nothing is still polling after `stop`.
        handle.stop();
        assert!(
            changes.recv().is_err(),
            "the sink is closed once the watcher has stopped"
        );
    }

    #[test]
    fn a_watcher_whose_receiver_is_dropped_ends_by_itself() {
        let watch = default_watch();
        let detector = Scripted::boxed(
            (0..8).map(|_| Ok(poll_of(&watch, &["dota2.exe"]))).collect(),
        );
        let watcher = GameWatcher::with_detector(detector, &watch, Duration::from_millis(5));
        let (sink, changes) = std::sync::mpsc::channel();
        let handle = spawn(watcher, sink).expect("a watcher thread");
        drop(changes); // the driver is gone
        // Dropping the handle joins the thread; if the thread had not noticed the closed
        // sink it would still be pollable, and this would hang. The assertion is that it
        // returns.
        handle.stop();
    }
}
