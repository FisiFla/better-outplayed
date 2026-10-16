//! Starting the game-event integrations and acting on what they derive.
//!
//! Both front-ends (the CLI and the desktop shell) need the same thing: run the League
//! poller and the CS2 / Dota 2 listener the `[events]` section asked for, and turn each event
//! they derive into a clip (a highlight) or a marker (everything else). It lives here, beside
//! the engine it drives, so the two cannot drift apart — the desktop shell used to have none
//! of it, which meant an event-triggered clip only ever happened under the CLI.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use localplay_events::gsi::{self, GsiConfig, GsiListener};
use localplay_events::lol::{self, LolConfig, LolHandle};
use localplay_events::{EventKind, GameEvent};
use serde::Deserialize;

use crate::{ClipReason, RecordedClip, Recorder};

/// `[events]` — which game-event sources to run (spec §7.1, §7.2).
///
/// Two settings, and no more: the endpoint the League poller uses is fixed by the spec, the
/// GSI path is fixed, and the token is generated into the application data directory rather
/// than written in a file a user edits (see `gsi::load_or_create_token`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EventsSection {
    /// Poll the League Live Client Data API (spec §7.1). Harmless when no game is running:
    /// a refused loopback connection is the normal state and is not an error.
    pub lol_poll_enabled: bool,
    /// The port the CS2 / Dota 2 GSI listener binds on loopback, or `0` to leave it off.
    pub gsi_port: u16,
    /// Event kinds that must **not** take a clip even though they are highlights — the tags
    /// from `EventKind::as_tag`, e.g. `["objective", "assist"]` for a player who wants their
    /// kills and not every turret. A muted kind is still recorded as a marker on the session
    /// timeline. Unknown tags are rejected at load, so a typo cannot silently mute nothing.
    #[serde(default)]
    pub mute_clips: Vec<String>,
}

impl EventsSection {
    /// The muted kinds, or the first tag that is not one.
    pub fn muted_kinds(&self) -> Result<Vec<EventKind>> {
        self.mute_clips
            .iter()
            .map(|tag| {
                EventKind::from_tag(tag).with_context(|| {
                    format!("events.mute_clips: `{tag}` is not an event kind (kill, death, assist, objective, bomb_planted, bomb_defused, game_end, ...)")
                })
            })
            .collect()
    }
}

/// What the driver does with events: how bursts coalesce, and which highlights stay markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub coalesce: Coalesce,
    pub muted: Vec<EventKind>,
}

impl Policy {
    pub fn new(events: &EventsSection, pre_seconds: u64) -> Result<Self> {
        Ok(Self { coalesce: Coalesce::for_pre_roll(pre_seconds), muted: events.muted_kinds()? })
    }

    /// Whether this event takes a clip: a highlight that has not been muted.
    pub fn clips(&self, event: &GameEvent) -> bool {
        event.is_highlight() && !self.muted.contains(&event.kind)
    }
}

impl Default for EventsSection {
    /// The values `config.example.toml` ships: a file with no `[events]` section behaves as
    /// the example does, which is also what the desktop shell relies on.
    fn default() -> Self {
        Self { lol_poll_enabled: true, gsi_port: 45671, mute_clips: Vec::new() }
    }
}

/// The event sources a front-end was configured to run, and the channel they report on.
///
/// Dropping this stops the sources: each handle joins its own thread (see the handles'
/// `Drop`), so nothing keeps polling a game or a port after the driver loop has ended. The
/// handles are held here rather than in the loop for exactly that reason.
pub struct EventSources {
    /// Derived events, in the order the sources produced them.
    pub events: Receiver<GameEvent>,
    /// Kept alive (not read): dropping it stops the poller.
    pub _lol: Option<LolHandle>,
    /// The same for the GSI listener.
    pub _gsi: Option<GsiListener>,
}

/// Start whatever `[events]` asked for, best effort.
///
/// Each source is independent and none is essential: a League poll is pointless on a machine
/// with no League install, and a GSI listener needs a port and a token file, either of which
/// can fail. A failure is a warning naming the setting, never a reason to refuse to record —
/// the hotkey still works, and that is the fallback spec §7.4 describes.
pub fn start_event_sources(app_dir: &Path, events: &EventsSection) -> EventSources {
    let (sink, events_rx) = mpsc::channel::<GameEvent>();

    let lol = if events.lol_poll_enabled {
        match lol::spawn(LolConfig::live_client(), sink.clone()) {
            Ok(handle) => {
                tracing::info!(
                    "League of Legends: polling {} for game events (set \
                     events.lol_poll_enabled = false to stop)",
                    handle.endpoint().url()
                );
                Some(handle)
            }
            Err(err) => {
                tracing::warn!("events.lol_poll_enabled is set but polling could not start: {err:#}");
                None
            }
        }
    } else {
        tracing::info!("League of Legends polling is disabled (events.lol_poll_enabled = false)");
        None
    };

    let gsi = if events.gsi_port == 0 {
        tracing::info!("the CS2 / Dota 2 listener is disabled (events.gsi_port = 0)");
        None
    } else {
        match start_gsi(app_dir, events.gsi_port, sink.clone()) {
            Ok(listener) => Some(listener),
            Err(err) => {
                tracing::warn!("events.gsi_port is set but the listener could not start: {err:#}");
                None
            }
        }
    };

    EventSources { events: events_rx, _lol: lol, _gsi: gsi }
}

/// Bring up the GSI listener and write out the file the user has to install (spec §7.2).
///
/// The application never writes into a game's installation directory: it generates the file
/// into its own data directory and says where to copy it. The token is loaded from (or
/// generated into) the data directory, so the file the user installed once keeps working
/// across runs — and it is never logged, only the path it lives at.
fn start_gsi(app_dir: &Path, port: u16, sink: localplay_events::EventSink) -> Result<GsiListener> {
    let token_path = app_dir.join("gsi-token.txt");
    let token = gsi::load_or_create_token(&token_path)?;

    // One file per game, in its own directory: the two games subscribe different data
    // blocks, and the file name has to be the same for both, so keeping them apart is the
    // only way not to hand the user the wrong file.
    let dirs = [
        ("cs2", gsi::integration_cfg(port, &token), "Counter-Strike 2 / CS:GO"),
        ("dota2", gsi::integration_cfg_dota(port, &token), "Dota 2"),
    ];
    for (game, text, name) in dirs {
        let dir = app_dir.join("gsi").join(game);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("gamestate_integration_localplay.cfg");
        std::fs::write(&path, text)
            .with_context(|| format!("writing {}", path.display()))?;
        tracing::info!(
            "{name}: copy {} into the game's cfg directory and restart the game",
            path.display()
        );
    }

    let listener = gsi::spawn(GsiConfig::new(port, token), sink)?;
    tracing::info!(
        "the token the game must carry is in {} (never logged, and not in the config file)",
        token_path.display()
    );
    Ok(listener)
}

/// Do what a derived event asks for.
///
/// The only decision here is highlight versus marker, and it is the vocabulary's
/// ([`localplay_events::EventKind::is_highlight`]) rather than this loop's. Everything else is one call.
pub fn act_on_event(recorder: &Recorder, event: GameEvent) -> Result<Option<RecordedClip>> {
    if event.is_highlight() {
        let described = format!("{} {}", event.source, event.kind);
        recorder
            .clip_now_with(ClipReason::GameEvent(event))
            .map(|clip| {
                tracing::info!("the {described} clip is {}", clip.metadata.path.display());
                Some(clip)
            })
            .with_context(|| format!("taking a clip for a {described} event"))
    } else {
        let described = format!("{} {}", event.source, event.kind);
        recorder
            .note_event(event)
            .map(|id| {
                tracing::info!("recorded the {described} marker as event #{id}");
                None
            })
            .with_context(|| format!("recording a {described} marker"))
    }
}


/// How long a burst of highlights is allowed to grow before it becomes one clip.
///
/// A pentakill is five kills inside ten seconds. Answering each with its own pre-roll clip
/// gave five overlapping files for one moment; waiting for the burst to settle and cutting
/// **one** clip over all of it gives one file that contains every kill. The cut happens
/// `quiet` after the last highlight, or `max` after the first, whichever is sooner — and
/// `max` is kept inside the pre-roll so the *first* event is still on the buffered side of
/// the cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coalesce {
    /// How long without another highlight ends a burst.
    pub quiet: Duration,
    /// The longest a burst may run from its first event.
    pub max: Duration,
}

impl Coalesce {
    /// Sized from the buffer's pre-roll: a burst may run for 60 % of it, and ends after a
    /// third of that without news (never under a second, never over six).
    pub fn for_pre_roll(pre_seconds: u64) -> Self {
        let max = Duration::from_millis(pre_seconds.saturating_mul(600));
        let quiet = (max / 3).clamp(Duration::from_secs(1), Duration::from_secs(6));
        Self { quiet, max: max.max(quiet) }
    }
}

/// Everything in one burst, `first` included, in arrival order.
///
/// Non-highlights (markers) that arrive meanwhile are collected but neither extend nor end
/// the burst. The channel closing ends it, because nothing further can arrive.
fn gather_burst(first: GameEvent, rx: &Receiver<GameEvent>, policy: &Policy) -> Vec<GameEvent> {
    let c = policy.coalesce;
    let started = std::time::Instant::now();
    let mut last_highlight = started;
    let mut burst = vec![first];
    loop {
        let now = std::time::Instant::now();
        let quiet_left = c.quiet.saturating_sub(now.duration_since(last_highlight));
        let max_left = c.max.saturating_sub(now.duration_since(started));
        let wait = quiet_left.min(max_left);
        if wait.is_zero() {
            return burst;
        }
        match rx.recv_timeout(wait) {
            Ok(event) => {
                if policy.clips(&event) {
                    last_highlight = std::time::Instant::now();
                }
                burst.push(event);
            }
            Err(_) => return burst,
        }
    }
}

/// [`act_on_event`] with burst coalescing: a highlight waits for its burst to settle, then
/// one clip is cut for the burst (reason: the first highlight) and every other event in it
/// is recorded as a marker, so nothing is lost from the session's timeline. Returns the clip
/// when one was taken.
pub fn act_on_burst(
    recorder: &Recorder,
    first: GameEvent,
    rx: &Receiver<GameEvent>,
    policy: &Policy,
) -> Result<Option<RecordedClip>> {
    if !policy.clips(&first) {
        // A marker, or a highlight the player muted: recorded on the timeline, no footage.
        return recorder
            .note_event(first)
            .map(|_| None)
            .context("recording a game-event marker");
    }
    let mut burst = gather_burst(first, rx, policy).into_iter();
    let head = burst.next().expect("a burst always holds its first event");
    let result = act_on_event(recorder, head);
    for rest in burst {
        // Marker-only: the clip above already covers these moments.
        if let Err(err) = recorder.note_event(rest) {
            tracing::warn!("a burst event could not be recorded as a marker: {err:#}");
        }
    }
    result
}

/// Called with every clip the driver takes, on the driver's thread — the desktop shell's way
/// to tell the player a clip landed without the recorder crate knowing about notifications.
pub type ClipHook = Box<dyn Fn(&RecordedClip) + Send + 'static>;

/// The thread that turns derived events into clips while a recording runs (the desktop
/// shell's counterpart of the CLI's driver loop).
///
/// Dropping it stops the thread and then the sources, so nothing keeps polling a game or
/// holding a port once the recording it served has ended.
pub struct EventDriver {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl EventDriver {
    /// Start the sources `events` asks for and drive `recorder` from them.
    pub fn spawn(
        recorder: Arc<Recorder>,
        app_dir: &Path,
        events: &EventsSection,
        policy: Policy,
        on_clip: ClipHook,
    ) -> Result<Self> {
        let sources = start_event_sources(app_dir, events);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("localplay-events".into())
            .spawn(move || drive(&recorder, sources, &flag, &policy, &on_clip))
            .context("spawning the game-event thread")?;
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for EventDriver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

const DRIVER_POLL: Duration = Duration::from_millis(250);

fn drive(
    recorder: &Recorder,
    sources: EventSources,
    stop: &AtomicBool,
    policy: &Policy,
    on_clip: &ClipHook,
) {
    while !stop.load(Ordering::Acquire) {
        match sources.events.recv_timeout(DRIVER_POLL) {
            Ok(event) => {
                // Nothing is being recorded yet (armed for a game, or already stopped): a
                // game event then is a state, not a failure.
                if !recorder.is_running() {
                    tracing::info!("a game event arrived while nothing is being recorded; ignored");
                    continue;
                }
                match act_on_burst(recorder, event, &sources.events, policy) {
                    Ok(Some(clip)) => on_clip(&clip),
                    Ok(None) => {}
                    Err(err) => tracing::warn!("the game event could not be acted on: {err:#}"),
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kill() -> GameEvent {
        GameEvent::bare(localplay_events::Source::Lol, localplay_events::EventKind::Kill)
    }

    fn quick() -> Policy {
        Policy {
            coalesce: Coalesce { quiet: Duration::from_millis(60), max: Duration::from_millis(400) },
            muted: Vec::new(),
        }
    }

    #[test]
    fn a_muted_highlight_does_not_take_a_clip_and_an_unknown_tag_is_an_error() {
        let events = EventsSection { mute_clips: vec!["objective".into()], ..EventsSection::default() };
        let policy = Policy::new(&events, 30).unwrap();
        let objective = GameEvent::bare(localplay_events::Source::Lol, EventKind::Objective);
        assert!(!policy.clips(&objective), "muted");
        assert!(policy.clips(&kill()), "an unmuted highlight still clips");
        let bad = EventsSection { mute_clips: vec!["kil".into()], ..EventsSection::default() };
        assert!(bad.muted_kinds().unwrap_err().to_string().contains("kil"));
    }

    #[test]
    fn a_burst_of_kills_is_gathered_into_one_group() {
        let (tx, rx) = mpsc::channel();
        for _ in 0..4 {
            tx.send(kill()).unwrap();
        }
        let burst = gather_burst(kill(), &rx, &quick());
        assert_eq!(burst.len(), 5, "the first plus the four that were already waiting");
    }

    #[test]
    fn a_lone_event_ends_after_the_quiet_gap_and_not_the_max() {
        let (_tx, rx) = mpsc::channel::<GameEvent>();
        let t = std::time::Instant::now();
        assert_eq!(gather_burst(kill(), &rx, &quick()).len(), 1);
        assert!(t.elapsed() < Duration::from_millis(300), "ended on the quiet gap");
    }

    #[test]
    fn a_continuous_stream_is_cut_at_the_max() {
        let (tx, rx) = mpsc::channel();
        let feeder = std::thread::spawn(move || {
            for _ in 0..40 {
                if tx.send(kill()).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let t = std::time::Instant::now();
        let burst = gather_burst(kill(), &rx, &quick());
        assert!(t.elapsed() < Duration::from_millis(700), "cut near max, not run to the end of the stream");
        assert!(burst.len() > 3);
        drop(rx);
        feeder.join().unwrap();
    }

    #[test]
    fn the_window_scales_with_the_pre_roll_and_stays_inside_it() {
        let c = Coalesce::for_pre_roll(30);
        assert_eq!(c.max, Duration::from_secs(18));
        assert_eq!(c.quiet, Duration::from_secs(6));
        let tiny = Coalesce::for_pre_roll(1);
        assert!(tiny.quiet >= Duration::from_secs(1));
        assert!(tiny.max >= tiny.quiet);
    }

    #[test]
    fn a_missing_events_section_means_the_example_values() {
        assert_eq!(EventsSection::default(), EventsSection { lol_poll_enabled: true, gsi_port: 45671, mute_clips: Vec::new() });
    }

    #[test]
    fn the_example_file_and_the_default_agree() {
        #[derive(Deserialize)]
        struct File {
            events: EventsSection,
        }
        let text = include_str!("../../../config.example.toml");
        let file: File = toml::from_str(text).expect("the example parses");
        assert_eq!(file.events, EventsSection::default());
    }
}
