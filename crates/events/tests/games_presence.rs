//! The game-presence API as a driver outside this crate sees it.
//!
//! `process.rs`'s own tests drive the logic directly; this file drives the **public
//! surface**, which is what the tray and the CLI actually call. That is a different
//! question, and it catches a different class of mistake: a type that is right but not
//! exported, a constructor a caller cannot reach, an opt-in that is a flag on a watcher
//! that runs anyway.
//!
//! # What this proves
//!
//! * the shipped watch list is the six titles, and League of Legends is the only one
//!   recognised by Riot's Live Client Data API — Valorant is a Riot title without one, and
//!   is watched by its game process instead;
//! * with `auto_record = false` (the default) **nothing is started at all**: no watcher, no
//!   poll, no snapshot, no request. The opt-in is real;
//! * with it on, the production path builds, runs and stops cleanly, and stopping releases
//!   the channel;
//! * a game already running when the watcher starts is reported by the first poll, once,
//!   and a game that goes on running is not reported again;
//! * the process watcher either answers (Windows) or refuses with a clear reason and never
//!   panics — the non-Windows half of the trait's contract.
//!
//! # What this does not prove
//!
//! The Windows enumeration itself, which needs a Windows machine to run: see the module
//! docs in `process.rs`. Nothing here touches a socket except the Live Client Data API
//! source, which addresses `127.0.0.1:2999` and nothing else (it is refused on a machine
//! with no game running, which is every machine this test has run on).

use localplay_events::process::{
    default_watch, GamePresence, GameWatcher, GamesSection, Presence, PresenceChange,
    ProcessWatcher, Signal, WatchedGame,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

/// A detector that answers the same thing every time, and counts how often it was asked.
///
/// Cheap on purpose: what is under test here is not detection but what the loop around a
/// detector does with the answers.
struct Always {
    games: Vec<WatchedGame>,
    running: Vec<String>,
    polls: Arc<AtomicUsize>,
}

impl GamePresence for Always {
    fn poll(&mut self) -> anyhow::Result<Vec<Presence>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .games
            .iter()
            .map(|game| Presence {
                game: game.clone(),
                running: self
                    .running
                    .iter()
                    .any(|exe| exe.eq_ignore_ascii_case(&game.exe)),
            })
            .collect())
    }
}

#[test]
fn the_shipped_watch_list_is_the_titles_the_project_names() {
    let watch = default_watch();
    let names: Vec<&str> = watch.iter().map(|game| game.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "League of Legends",
            "Counter-Strike 2",
            "Dota 2",
            "Rainbow Six Siege",
            "Valorant",
            "Overwatch 2",
        ]
    );

    // The distinction that matters: the Live Client Data API is Riot's, and Riot publishes
    // it for League. Valorant is a Riot title that publishes none, so the watch list says
    // `process` for it — a lie would be worse than a mechanism.
    let by_api: Vec<&str> = watch
        .iter()
        .filter(|game| game.signal == Signal::LiveClientApi)
        .map(|game| game.name.as_str())
        .collect();
    assert_eq!(by_api, vec!["League of Legends"]);
    assert_eq!(watch.iter().filter(|game| game.signal == Signal::Process).count(), 5);
    assert!(watch
        .iter()
        .any(|game| game.exe == "VALORANT-Win64-Shipping.exe" && game.signal == Signal::Process));

    // Every title is watched by a *game* process, not by a launcher's.
    for launcher in ["LeagueClient.exe", "VALORANT.exe", "steam.exe"] {
        assert!(
            watch.iter().all(|game| !game.exe.eq_ignore_ascii_case(launcher)),
            "{launcher} is a launcher, and launchers are open all evening"
        );
    }
}

#[test]
fn auto_record_off_starts_nothing_and_therefore_polls_nothing() {
    let (sink, changes) = channel();
    let started = GamesSection::default().start(sink).expect("the default section cannot fail");
    assert!(
        started.is_none(),
        "auto_record defaults to false, so the default section must start no watcher"
    );
    drop(started);

    // Nothing was started, so the sink went with it: that is the whole safety property —
    // with the feature off, not even a process list is enumerated.
    match changes.recv_timeout(Duration::from_secs(1)) {
        Err(RecvTimeoutError::Disconnected) => {}
        other => panic!("nothing should have been watching, but the channel said {other:?}"),
    }
}

#[test]
fn auto_record_on_starts_a_watcher_that_stops_cleanly() {
    let section = GamesSection { auto_record: true, poll_ms: 250, ..GamesSection::default() };
    assert!(section.watching());
    let (sink, changes) = channel();
    let handle = section
        .start(sink)
        .expect("a watcher builds")
        .expect("auto_record is on, so a watcher exists");

    // Let it poll at least once. A build machine is not running any of these games, so
    // there is normally nothing to receive — and if the machine *is* mid-match (a
    // developer's own box), whatever arrives is drained rather than asserted about.
    std::thread::sleep(Duration::from_millis(400));
    handle.stop();

    let ended = loop {
        match changes.recv_timeout(Duration::from_secs(2)) {
            Ok(change) => assert!(change.is_start() || change.is_stop()),
            Err(err) => break err,
        }
    };
    assert!(
        matches!(ended, RecvTimeoutError::Disconnected),
        "stopping the watcher releases the sink, so nothing is polling after it: {ended:?}"
    );
}

#[test]
fn a_game_already_running_is_reported_by_the_first_poll_and_only_once() {
    let watch = default_watch();
    let polls = Arc::new(AtomicUsize::new(0));
    let detector = Always {
        games: watch.clone(),
        running: vec!["dota2.exe".to_string()],
        polls: Arc::clone(&polls),
    };
    let mut watcher = GameWatcher::with_detector(Box::new(detector), &watch, Duration::from_millis(10));

    let first = watcher.poll_once();
    assert_eq!(first.len(), 1, "a game that was already running is running now: one start");
    assert_eq!(first[0].game().name, "Dota 2");
    assert!(first[0].is_start());
    assert!(!first[0].is_stop());

    for _ in 0..5 {
        assert!(
            watcher.poll_once().is_empty(),
            "a game that goes on running does not start over and over"
        );
    }
    assert_eq!(watcher.running().len(), 1);
    assert_eq!(polls.load(Ordering::SeqCst), 6, "one detector poll per poll_once");
}

#[test]
fn the_watcher_thread_reports_a_start_and_releases_the_sink_when_stopped() {
    let watch = default_watch();
    let detector = Always {
        games: watch.clone(),
        running: vec!["cs2.exe".to_string()],
        polls: Arc::new(AtomicUsize::new(0)),
    };
    let watcher = GameWatcher::with_detector(Box::new(detector), &watch, Duration::from_millis(5));
    let (sink, changes) = channel();

    let handle = localplay_events::process::spawn(watcher, sink).expect("a watcher thread");
    let change = changes.recv_timeout(Duration::from_secs(5)).expect("the start arrives");
    assert_eq!(change.game().name, "Counter-Strike 2");
    assert_eq!(change.to_string(), "Counter-Strike 2 (cs2.exe) started");

    handle.stop();
    assert!(
        changes.recv().is_err(),
        "after stop the thread is joined and its end of the channel is gone"
    );
}

#[test]
fn the_process_watcher_answers_on_windows_and_refuses_clearly_elsewhere() {
    // The non-Windows half of the trait's contract: a clear error, never a panic and never
    // a silent empty list that a driver would read as "every game has stopped".
    let mut watcher = ProcessWatcher::new(default_watch()).expect("a validated watch list");
    let answer = watcher.poll();

    if cfg!(windows) {
        let presences = answer.expect("the process-list snapshot is what Windows is for");
        assert_eq!(presences.len(), default_watch().len(), "one answer per watched title");
    } else {
        let err = answer.expect_err("this platform has no process-list snapshot to take");
        let message = err.to_string();
        assert!(
            message.contains("Windows"),
            "the refusal has to name what is missing: {message}"
        );
    }
}

#[test]
fn a_change_says_what_happened_for_a_log_line() {
    let game = WatchedGame::by_process("Counter-Strike 2", "cs2.exe");
    let started = PresenceChange::Started(game.clone());
    assert_eq!(started.to_string(), "Counter-Strike 2 (cs2.exe) started");
    assert!(started.is_start() && !started.is_stop());
    assert_eq!(started.game(), &game);

    let stopped = PresenceChange::Stopped(game.clone());
    assert_eq!(stopped.to_string(), "Counter-Strike 2 (cs2.exe) stopped");
    assert!(stopped.is_stop() && !stopped.is_start());
}

#[test]
fn a_watch_list_a_user_could_have_written_by_hand_is_accepted() {
    // The shape `config.toml` documents, built through the public constructors.
    let watch = vec![
        WatchedGame::by_live_client_api("League of Legends", "League of Legends.exe"),
        WatchedGame::by_process("Counter-Strike 2", "cs2.exe"),
    ];
    let section = GamesSection { auto_record: true, poll_ms: 5_000, watch: watch.clone() };
    assert_eq!(section.interval(), Duration::from_millis(5_000));
    assert!(ProcessWatcher::new(watch).is_ok());
}
