//! Starting the game-event integrations and acting on what they derive.
//!
//! Both front-ends run the League poller and the CS2 / Dota 2 listener the `[events]` section asked
//! for, and turn each derived event into a clip (a highlight) or a marker. It lives beside the
//! engine it drives so the two front-ends cannot drift apart.

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

use crate::{ClipReason, ClipWindow, RecordedClip, Recorder};

/// `[events]`: which game-event sources to run.
///
/// The League endpoint and the GSI path are fixed; the GSI token is generated into the application
/// data directory rather than a user-edited file (see `gsi::load_or_create_token`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EventsSection {
    /// Poll the League Live Client Data API. Harmless with no game running: a refused loopback
    /// connection is the normal state.
    pub lol_poll_enabled: bool,
    /// The port the CS2 / Dota 2 GSI listener binds on loopback, or `0` to leave it off.
    pub gsi_port: u16,
    /// Highlight kinds that must not take a clip (tags from `EventKind::as_tag`, e.g.
    /// `["objective", "assist"]`). A muted kind is still recorded as a marker. Unknown tags are
    /// rejected at load, so a typo cannot silently mute nothing.
    #[serde(default)]
    pub mute_clips: Vec<String>,
    /// `[events.lol]`: clip length for League events; unset keys use `[buffer]`.
    #[serde(default)]
    pub lol: GameClipSection,
    /// `[events.gsi]`: clip length for CS2 / Dota 2 events (one listener serves both).
    #[serde(default)]
    pub gsi: GameClipSection,
}

/// A game's own clip length: `pre_seconds` before the event and `post_seconds` after.
///
/// Either key may be omitted (that half uses `[buffer]`); values above `[buffer] max_pre_seconds` /
/// `max_post_seconds` are clamped, not refused (see [`ClipWindow`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct GameClipSection {
    pub pre_seconds: Option<u64>,
    pub post_seconds: Option<u64>,
}

impl GameClipSection {
    pub fn window(self) -> ClipWindow {
        ClipWindow { pre_seconds: self.pre_seconds, post_seconds: self.post_seconds }
    }
}

impl EventsSection {
    /// The muted kinds, or the first tag that is not one.
    pub fn muted_kinds(&self) -> Result<Vec<EventKind>> {
        self.mute_clips
            .iter()
            .map(|tag| {
                EventKind::from_tag(tag).with_context(|| {
                    format!("events.mute_clips: `{tag}` is not an event kind (kill, death, assist, objective, dragon, baron, herald, turret, inhibitor, bomb_planted, bomb_defused, game_end, ...)")
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
    /// Per-game clip lengths (`[events.lol]`, `[events.gsi]`).
    pub lol: ClipWindow,
    pub gsi: ClipWindow,
}

impl Policy {
    pub fn new(events: &EventsSection, pre_seconds: u64) -> Result<Self> {
        Ok(Self {
            coalesce: Coalesce::for_pre_roll(pre_seconds),
            muted: events.muted_kinds()?,
            lol: events.lol.window(),
            gsi: events.gsi.window(),
        })
    }

    /// Whether this event takes a clip: a highlight that is not muted. Muting `objective` mutes every
    /// objective sub-kind; muting a sub-kind mutes only that one.
    pub fn clips(&self, event: &GameEvent) -> bool {
        event.is_highlight()
            && !self.muted.contains(&event.kind)
            && !(event.kind.is_objective() && self.muted.contains(&EventKind::Objective))
    }

    /// The clip length for an event from `source`: the game's own, else the buffer's.
    pub fn window_for(&self, source: localplay_events::Source) -> ClipWindow {
        match source {
            localplay_events::Source::Lol => self.lol,
            localplay_events::Source::Gsi => self.gsi,
            localplay_events::Source::Manual => ClipWindow::default(),
        }
    }
}

impl Default for EventsSection {
    /// The values `config.example.toml` ships, so a file with no `[events]` section behaves as the example does.
    fn default() -> Self {
        Self {
            lol_poll_enabled: true,
            gsi_port: 45671,
            mute_clips: Vec::new(),
            lol: GameClipSection::default(),
            gsi: GameClipSection::default(),
        }
    }
}

/// The event sources a front-end was configured to run, and the channel they report on.
///
/// Dropping this stops the sources: each handle joins its own thread on `Drop`, so they are held
/// here so nothing keeps polling a game or a port after the driver loop ends.
pub struct EventSources {
    /// Derived events, in the order the sources produced them.
    pub events: Receiver<GameEvent>,
    /// Kept alive (not read): dropping it stops the poller.
    pub _lol: Option<LolHandle>,
    /// The same for the GSI listener.
    pub _gsi: Option<GsiListener>,
    /// What starting each source concluded, for a health readout; empty when assembled by hand.
    pub reports: Vec<SourceReport>,
}

/// How a source came out of startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceState {
    Running,
    Disabled,
    Failed,
}

/// One source's startup outcome. `detail` is user-facing and never carries the GSI token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceReport {
    pub source: &'static str,
    pub state: SourceState,
    pub detail: String,
}

/// Start whatever `[events]` asked for, best effort.
///
/// Sources are independent and none is essential (no League install, or a GSI port or token file
/// that fails). A failure is a warning naming the setting, never a reason to refuse to record;
/// the hotkey is the fallback.
pub fn start_event_sources(app_dir: &Path, events: &EventsSection) -> EventSources {
    let (sink, events_rx) = mpsc::channel::<GameEvent>();

    let mut reports = Vec::new();
    let lol = if events.lol_poll_enabled {
        match lol::spawn(LolConfig::live_client(), sink.clone()) {
            Ok(handle) => {
                tracing::info!(
                    "League of Legends: polling {} for game events (set \
                     events.lol_poll_enabled = false to stop)",
                    handle.endpoint().url()
                );
                reports.push(SourceReport {
                    source: "League of Legends",
                    state: SourceState::Running,
                    detail: format!("polling {}", handle.endpoint().url()),
                });
                Some(handle)
            }
            Err(err) => {
                tracing::warn!("events.lol_poll_enabled is set but polling could not start: {err:#}");
                reports.push(SourceReport {
                    source: "League of Legends",
                    state: SourceState::Failed,
                    detail: format!("{err:#}"),
                });
                None
            }
        }
    } else {
        tracing::info!("League of Legends polling is disabled (events.lol_poll_enabled = false)");
        reports.push(SourceReport {
            source: "League of Legends",
            state: SourceState::Disabled,
            detail: "events.lol_poll_enabled = false".into(),
        });
        None
    };

    let gsi = if events.gsi_port == 0 {
        tracing::info!("the CS2 / Dota 2 listener is disabled (events.gsi_port = 0)");
        reports.push(SourceReport {
            source: "CS2 / Dota 2",
            state: SourceState::Disabled,
            detail: "events.gsi_port = 0".into(),
        });
        None
    } else {
        match start_gsi(app_dir, events.gsi_port, sink.clone()) {
            Ok(listener) => {
                reports.push(SourceReport {
                    source: "CS2 / Dota 2",
                    state: SourceState::Running,
                    detail: format!("listening on 127.0.0.1:{}", events.gsi_port),
                });
                Some(listener)
            }
            Err(err) => {
                tracing::warn!("events.gsi_port is set but the listener could not start: {err:#}");
                reports.push(SourceReport {
                    source: "CS2 / Dota 2",
                    state: SourceState::Failed,
                    detail: format!("{err:#}"),
                });
                None
            }
        }
    };

    EventSources { events: events_rx, _lol: lol, _gsi: gsi, reports }
}

/// Bring up the GSI listener and write out the file the user has to install.
///
/// Never writes into a game's installation directory: it generates the file into its own data
/// directory and says where to copy it (the desktop settings button is the one user-triggered
/// installer). The token is persisted so the installed file keeps working across runs, and is
/// never logged.
fn start_gsi(app_dir: &Path, port: u16, sink: localplay_events::EventSink) -> Result<GsiListener> {
    let token_path = app_dir.join("gsi-token.txt");
    let token = gsi::load_or_create_token(&token_path)?;

    // One file per game in its own directory: the games subscribe different data blocks but the file name is the same.
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

/// Do what a derived event asks for. The only decision, highlight versus marker, is the
/// vocabulary's ([`localplay_events::EventKind::is_highlight`]).
pub fn act_on_event(recorder: &Recorder, event: GameEvent) -> Result<Option<RecordedClip>> {
    act_on_event_in(recorder, event, ClipWindow::default())
}

/// [`act_on_event`] with the clip length the event's game asked for.
pub fn act_on_event_in(
    recorder: &Recorder,
    event: GameEvent,
    window: ClipWindow,
) -> Result<Option<RecordedClip>> {
    if event.is_highlight() {
        let described = format!("{} {}", event.source, event.kind);
        recorder
            .clip_now_in(ClipReason::GameEvent(event), window)
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


/// How long a burst of highlights may grow before it becomes one clip.
///
/// A pentakill is five kills in ten seconds; one clip over the settled burst replaces five
/// overlapping files. The cut happens `quiet` after the last highlight or `max` after the first,
/// whichever is sooner, and `max` stays inside the pre-roll so the first event is still buffered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coalesce {
    /// How long without another highlight ends a burst.
    pub quiet: Duration,
    /// The longest a burst may run from its first event.
    pub max: Duration,
}

impl Coalesce {
    /// Sized from the buffer's pre-roll: a burst may run 60% of it and ends after a third of that
    /// without news (never under a second, never over six).
    pub fn for_pre_roll(pre_seconds: u64) -> Self {
        let max = Duration::from_millis(pre_seconds.saturating_mul(600));
        let quiet = (max / 3).clamp(Duration::from_secs(1), Duration::from_secs(6));
        Self { quiet, max: max.max(quiet) }
    }
}

/// Everything in one burst, `first` included, in arrival order. Markers arriving meanwhile are
/// collected but neither extend nor end the burst; the channel closing ends it.
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

/// [`act_on_event`] with burst coalescing: a highlight waits for its burst to settle, then one clip
/// is cut (reason: the first highlight) and every other event is recorded as a marker. Returns the clip if one was taken.
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
    let window = policy.window_for(head.source);
    let result = act_on_event_in(recorder, head, window);
    for rest in burst {
        // Marker-only: the clip above already covers these moments.
        if let Err(err) = recorder.note_event(rest) {
            tracing::warn!("a burst event could not be recorded as a marker: {err:#}");
        }
    }
    result
}

/// Called with every clip the driver takes, on the driver's thread, so the desktop shell can notify
/// the player without the recorder knowing about notifications.
pub type ClipHook = Box<dyn Fn(&RecordedClip) + Send + 'static>;

/// The thread that turns derived events into clips while a recording runs. Dropping it stops the
/// thread and then the sources.
pub struct EventDriver {
    reports: Vec<SourceReport>,
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
        let reports = sources.reports.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("localplay-events".into())
            .spawn(move || drive(&recorder, sources, &flag, &policy, &on_clip))
            .context("spawning the game-event thread")?;
        Ok(Self { reports, stop, thread: Some(thread) })
    }
}

impl EventDriver {
    /// How each source came out of startup, for the health page.
    pub fn reports(&self) -> &[SourceReport] {
        &self.reports
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
                // Nothing is recording (armed for a game, or stopped): an event then is a state, not a failure.
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
            lol: ClipWindow::default(),
            gsi: ClipWindow::default(),
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
    fn muting_objective_mutes_every_sub_kind_and_a_sub_kind_mutes_only_itself() {
        let all = EventsSection { mute_clips: vec!["objective".into()], ..EventsSection::default() };
        let policy = Policy::new(&all, 30).unwrap();
        for kind in EventKind::ALL.into_iter().filter(|k| k.is_objective()) {
            assert!(!policy.clips(&GameEvent::bare(localplay_events::Source::Lol, kind)), "{kind}");
        }
        let some = EventsSection { mute_clips: vec!["turret".into()], ..EventsSection::default() };
        let policy = Policy::new(&some, 30).unwrap();
        let clips = |kind| policy.clips(&GameEvent::bare(localplay_events::Source::Lol, kind));
        assert!(!clips(EventKind::Turret));
        assert!(clips(EventKind::Dragon) && clips(EventKind::Baron) && clips(EventKind::Objective));
    }

    #[test]
    fn per_game_clip_lengths_parse_and_reach_the_policy() {
        let events: EventsSection = toml::from_str(
            "lol_poll_enabled = true\ngsi_port = 0\n[lol]\npre_seconds = 25\npost_seconds = 8\n[gsi]\npre_seconds = 6\n",
        )
        .unwrap();
        let policy = Policy::new(&events, 30).unwrap();
        assert_eq!(policy.window_for(localplay_events::Source::Lol), ClipWindow::new(25, 8));
        assert_eq!(
            policy.window_for(localplay_events::Source::Gsi),
            ClipWindow { pre_seconds: Some(6), post_seconds: None }
        );
        assert_eq!(policy.window_for(localplay_events::Source::Manual), ClipWindow::default());
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
        // With a `max` an hour away, only the quiet gap can end the burst: returning at all is
        // the proof, so no wall-clock bound is asserted (a loaded runner would flake on one).
        let mut policy = quick();
        policy.coalesce.max = Duration::from_secs(3_600);
        let (_tx, rx) = mpsc::channel::<GameEvent>();
        assert_eq!(gather_burst(kill(), &rx, &policy).len(), 1);
    }

    #[test]
    fn a_continuous_stream_is_cut_at_the_max() {
        // The stream never goes quiet and never ends until the receiver is dropped, so the
        // only way `gather_burst` returns is the max cutting it. No elapsed-time bound: a
        // run-to-the-end implementation hangs here, which fails the test without a clock.
        let (tx, rx) = mpsc::channel();
        let feeder = std::thread::spawn(move || {
            while tx.send(kill()).is_ok() {
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let burst = gather_burst(kill(), &rx, &quick());
        assert!(burst.len() > 1, "the burst gathered the stream until the cut");
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
        assert_eq!(
            EventsSection::default(),
            EventsSection {
                lol_poll_enabled: true,
                gsi_port: 45671,
                mute_clips: Vec::new(),
                lol: GameClipSection::default(),
                gsi: GameClipSection::default(),
            }
        );
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
