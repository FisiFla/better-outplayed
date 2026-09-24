//! The clip index: opening it, inserting a clip that was just spliced, and applying the
//! storage policy to it.
//!
//! Everything here is synchronous bookkeeping over a [`Store`] and costs the capture path
//! nothing: the index is opened once at startup, written once per clip, and swept every
//! [`crate::CLEANUP_INTERVAL`]. It lives beside the engine rather than inside it because
//! it is the one part of the trigger path that can be tested without ffmpeg, a scratch
//! directory or a capture backend.

use crate::config::StorageSection;
use anyhow::{Context, Result};
use localplay_events::GameEvent;
use localplay_replay::splice::ClipMetadata;
use localplay_store::retention::{
    execute_retention, plan_retention, RetentionOutcome, RetentionPolicy, RetentionRules,
};
use localplay_store::{NewClip, NewEvent, Store};
use std::path::{Path, PathBuf};

/// Wall-clock now, in ms since the Unix epoch — the clock `clips.created_at` is on, and
/// so the clock the storage policy's age rule is evaluated against.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Wall-clock now, in whole seconds since the Unix epoch — the clock a clip file's name
/// is stamped from (`clip-<ts>.mp4`).
pub fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Open the clip index in the app data directory and bring its schema up to date
/// (spec §5.5).
///
/// A failure here is fatal, deliberately. An index that cannot be opened is an index that
/// records nothing, and a clip that is never recorded can neither be listed nor ever
/// cleaned up — the growing, unmanaged clips directory the storage policy exists to
/// prevent. Stopping with the path in the log is diagnosable; carrying on is not.
pub fn open_clip_index(db_path: &Path) -> Result<Store> {
    if let Some(dir) = db_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let store = Store::open(db_path)?;
    store.migrate()?;
    tracing::info!(
        "clip index {} (schema v{}): {} clips, {} bytes",
        db_path.display(),
        store.schema_version()?,
        store.list_clips()?.len(),
        store.total_bytes()?
    );
    Ok(store)
}

/// Insert the `clips` row for a clip that has just been spliced (spec §6.2 step 6).
///
/// * `started_at_ms` is the clip's first frame on the ring's **ledger timeline** (media
///   time), not a wall-clock instant: it is the only position the splice knows, and it is
///   the timeline the segment numbering — and therefore the footage — is measured on. The
///   wall-clock instant of the record is `created_at`, stamped by the store.
/// * The codec is `clip.encoder`, the encoder that actually produced the file
///   (`h264_nvenc`, `libx264`, …), because that is the name criterion 5 checks the file
///   against; the config's abstract `h264` would name neither.
/// * `session_id` is left NULL. The schema allows it, and this engine does not open a
///   `sessions` row: a recording session has no clean shutdown that stamps `ended_at` on
///   one, so a session row could never be finished. NULL is the honest value; a made-up id
///   would point at a session that does not exist.
///
/// A failed insert is reported at ERROR and returns `None` — it is *not* fatal. The clip
/// file exists and the user asked for it; losing it to a bookkeeping failure would be far
/// worse than an index that is missing an entry (spec §8.2 is about never losing a file,
/// not about perfect bookkeeping). The consequence is named in the log: an unindexed clip
/// is invisible to the list and to the storage policy.
pub fn index_clip(store: &Store, clip: &ClipMetadata, started_at_ms: u64) -> Option<i64> {
    let new = NewClip {
        path: clip.path.clone(),
        started_at_ms,
        duration_ms: clip.duration_ms,
        size_bytes: clip.size_bytes,
        codec: clip.encoder.clone(),
    };
    match store.insert_clip(&new) {
        Ok(id) => {
            tracing::info!(
                "indexed clip #{id} (media t={started_at_ms}ms, {}ms, {} bytes, {}): {}",
                clip.duration_ms,
                clip.size_bytes,
                clip.encoder,
                clip.path.display()
            );
            Some(id)
        }
        Err(err) => {
            tracing::error!(
                "could not index the clip that was just written ({}): {err:#}. The file is \
                 kept — it is on disk and it is what was asked for — but it will not appear \
                 in the clip list, and the storage policy cannot manage it.",
                clip.path.display()
            );
            None
        }
    }
}

/// The `events.kind` a manual clip records: the user pressed the hotkey, and the timeline
/// should show the moment they chose.
///
/// A manual clip used to record nothing but the clip, so a session's timeline carried the
/// game's kills and none of the user's own marks — the scrubber had nothing to plot for the
/// moments that mattered most. The tag is the same vocabulary the scrubber colours by
/// (`localplay-store` passes it through verbatim), and it is deliberately not a
/// [`GameEvent`]: nothing produced it but a keypress.
pub const BOOKMARK_KIND: &str = "bookmark";

/// Record the `events` row for a manual clip and return its id.
///
/// `payload` is `None`: a bookmark has no source to describe, and inventing one would make
/// the timeline claim a provenance it does not have.
pub fn index_bookmark(
    store: &Store,
    at_ms: u64,
    clip_id: Option<i64>,
    session_id: Option<i64>,
) -> Result<i64> {
    let id = store.insert_event(&NewEvent {
        session_id,
        kind: BOOKMARK_KIND.to_string(),
        at_ms,
        payload: None,
        clip_id,
    })?;
    tracing::info!("recorded bookmark event #{id} at media t={at_ms}ms against clip #{clip_id:?}");
    Ok(id)
}

/// Insert the `events` row for a derived game event (spec §5.5), linked to the clip it
/// produced when it produced one.
///
/// * `at_ms` is the event's position on the **ledger's media timeline** — the same clock
///   `clips.started_at` and a session's `media_epoch_ms` are on, so a session timeline can
///   place a marker against a clip without converting anything. For a clip's own event this
///   is the *trigger* instant, which is `buffer.pre_seconds` into the clip, because that is
///   the moment the event happened; the window merely starts earlier.
/// * `clip_id` is `None` for a marker (a game or round boundary, recorded but not clipped —
///   see `EventKind::is_highlight`) and for an event whose clip could not be indexed. The
///   column is nullable for exactly those cases.
/// * `session_id` is the session that was recording. It was left NULL here for as long as
///   "this engine opens no `sessions` row" was true, which stopped being true in Phase 5 —
///   so NULL now means an event detached from its session, which is what deletion does to
///   it, and not "there was no session".
///
/// Returns the new row's id, or the error: the callers differ on what a failure means. A
/// clip that has already been written must not be lost to a bookkeeping failure, while a
/// marker is the *only* thing the caller asked for, so failing it is worth reporting.
pub fn index_event(
    store: &Store,
    event: &GameEvent,
    at_ms: u64,
    clip_id: Option<i64>,
    session_id: Option<i64>,
) -> Result<i64> {
    let new = NewEvent {
        session_id,
        kind: event.kind.as_tag().to_string(),
        at_ms,
        payload: event.payload.clone(),
        clip_id,
    };
    let id = store.insert_event(&new)?;
    match clip_id {
        Some(clip) => tracing::info!(
            "recorded {} event #{} at media t={at_ms}ms against clip #{clip}",
            event.kind,
            id
        ),
        None => tracing::info!(
            "recorded {} event #{} at media t={at_ms}ms (no clip: a marker, not a highlight)",
            event.kind,
            id
        ),
    }
    Ok(id)
}

/// Bookkeeping across storage-policy passes, so a condition that lasts the whole session
/// is stated when it appears (or when its size changes) rather than once per pass.
///
/// One field per cap: clips and sessions are planned independently (spec §8.1), so each can
/// be unsatisfiable on its own, and a single field would let one class's recovery hide the
/// other's shortfall. Both are public because they are the observable "why did nothing get
/// deleted" of a pass, and a test asserts on them.
#[derive(Default, Debug)]
pub struct CleanupReport {
    /// The shortfall reported by the last pass whose clips cap could not be met; `None`
    /// while that cap is met, so a recurrence warns again.
    pub last_clips_shortfall: Option<u64>,
    /// The same for the sessions cap.
    pub last_sessions_shortfall: Option<u64>,
}

/// Apply the storage policy once (spec §8.1): plan against the index, execute the plan, and
/// report what happened.
///
/// The decision is [`plan_retention`]'s — one plan over **both** libraries, clips and
/// sessions, each with its own cap and age limit — and the deletion ordering is
/// [`execute_retention`]'s; this function supplies the policy from the config and the clock,
/// and decides what is worth a log line. It is silent when there is nothing to do, because it
/// runs at startup and then every [`crate::CLEANUP_INTERVAL`], and an idle pass is not news.
///
/// The returned [`RetentionOutcome`] is what the pass measured after executing the plan
/// (bytes reclaimed file by file, per class): returned rather than only logged so a caller
/// — and a test — can see the numbers instead of reconstructing them from log lines.
pub fn cleanup_pass(
    store: &Store,
    storage: &StorageSection,
    report: &mut CleanupReport,
) -> RetentionOutcome {
    let policy = RetentionPolicy {
        clips: RetentionRules {
            max_total_bytes: storage.max_total_bytes,
            // The retention rules count days as `u32`; the config keeps them as `u64` (it
            // always has). A day count above `u32::MAX` is not a policy, it is "never",
            // and clamping says so rather than wrapping to a small number.
            max_age_days: storage.max_age_days.min(u64::from(u32::MAX)) as u32,
        },
        sessions: RetentionRules {
            max_total_bytes: storage.sessions.max_total_bytes,
            max_age_days: storage.sessions.max_age_days.min(u64::from(u32::MAX)) as u32,
        },
    };

    let clips = match store.list_clips() {
        Ok(clips) => clips,
        Err(err) => {
            tracing::warn!("storage policy: cannot read the clip index: {err:#}");
            return RetentionOutcome::default();
        }
    };
    let sessions = match store.list_sessions() {
        Ok(sessions) => sessions,
        Err(err) => {
            tracing::warn!("storage policy: cannot read the session store: {err:#}");
            return RetentionOutcome::default();
        }
    };

    let plan = plan_retention(&clips, &sessions, &policy, now_ms());
    let outcome = match execute_retention(store, &plan) {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::warn!("storage policy: the pass could not be completed: {err:#}");
            return RetentionOutcome::default();
        }
    };

    if outcome.deleted() > 0 {
        tracing::info!(
            "storage policy: deleted {} clip(s) and {} session(s), reclaimed {} bytes; the \
             clips directory now holds {} bytes against its {} byte cap, and the session \
             store {} bytes against its {} byte cap",
            outcome.clips.deleted,
            outcome.sessions.deleted,
            outcome.bytes_reclaimed(),
            outcome.clips.bytes_after,
            policy.clips.max_total_bytes,
            outcome.sessions.bytes_after,
            policy.sessions.max_total_bytes
        );
    }
    let failed = outcome.clips.failed + outcome.sessions.failed;
    if failed > 0 {
        tracing::warn!(
            "storage policy: {failed} planned deletion(s) could not be applied; the affected \
             library is larger than the policy asked for"
        );
    }
    let leftovers: Vec<&PathBuf> = outcome.leftovers().collect();
    if !leftovers.is_empty() {
        // The row is gone (the delete committed) and the path is still there. Every entry is
        // one of: a removal that failed, a path deliberately refused (a symlinked or
        // suspicious session directory), or a file that was already missing. Named, because
        // "the library is within its cap" would otherwise be a statement about rows only.
        tracing::warn!(
            "storage policy: {} path(s) named by deleted rows are still on disk: {}",
            leftovers.len(),
            leftovers
                .iter()
                .take(5)
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // A cap that the immune members alone exceed is not merely "not met this pass" — no
    // pass can meet it: favourites and a *running* session are exempt (spec §8.1), so the
    // shortfall is what the user has to act on. Worth a warning, but only when it appears or
    // changes: the condition is permanent, and repeating it verbatim every pass would bury
    // everything else in the log.
    report_clips_shortfall(report, policy.clips.max_total_bytes, &plan, &outcome);
    report_sessions_shortfall(report, policy.sessions.max_total_bytes, &plan, &outcome);
    outcome
}

/// The clips cap's unsatisfiable case (see [`cleanup_pass`]).
fn report_clips_shortfall(
    report: &mut CleanupReport,
    cap: u64,
    plan: &localplay_store::retention::RetentionPlan,
    outcome: &RetentionOutcome,
) {
    if outcome.clips.cap_met {
        report.last_clips_shortfall = None;
    } else if report.last_clips_shortfall != Some(plan.clips.over_cap_by_bytes) {
        tracing::warn!(
            "storage policy: storage.max_total_bytes ({cap}) cannot be satisfied — the \
             favourited clips alone exceed it by {} bytes, and favourites are exempt from \
             both rules, so nothing is deleted for it. The clips directory holds {} bytes. \
             Raise storage.max_total_bytes or un-favourite some clips.",
            plan.clips.over_cap_by_bytes,
            outcome.clips.bytes_after
        );
        report.last_clips_shortfall = Some(plan.clips.over_cap_by_bytes);
    }
}

/// The sessions cap's unsatisfiable case — the same condition, but its two causes are both
/// "nothing here may be deleted": a favourited session, and a session that is still
/// recording (its scratch directory is footage that exists nowhere else).
fn report_sessions_shortfall(
    report: &mut CleanupReport,
    cap: u64,
    plan: &localplay_store::retention::RetentionPlan,
    outcome: &RetentionOutcome,
) {
    if outcome.sessions.cap_met {
        report.last_sessions_shortfall = None;
    } else if report.last_sessions_shortfall != Some(plan.sessions.over_cap_by_bytes) {
        tracing::warn!(
            "storage policy: storage.sessions.max_total_bytes ({cap}) cannot be satisfied — \
             the sessions nothing may delete (favourited ones, and one that is still \
             recording) exceed it by {} bytes. The session store holds {} bytes. Raise \
             storage.sessions.max_total_bytes, un-favourite a session, or finish the \
             recording that is running.",
            plan.sessions.over_cap_by_bytes,
            outcome.sessions.bytes_after
        );
        report.last_sessions_shortfall = Some(plan.sessions.over_cap_by_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The clip metadata a ring's `trigger` hands back, without running ffmpeg.
    fn metadata(path: PathBuf, size_bytes: u64) -> ClipMetadata {
        ClipMetadata { path, duration_ms: 12_000, size_bytes, encoder: "h264_nvenc".into() }
    }

    /// An open index in a temp directory, plus the directory holding it.
    fn index_dir() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = open_clip_index(&dir.path().join("localplay.db")).expect("open the index");
        (dir, store)
    }

    /// The schema version this build's store writes, **read from the store itself**.
    ///
    /// `localplay-store` keeps its `SCHEMA_VERSION` private, so there is no exported
    /// constant to assert against — and a bare `2` here is exactly what broke when the
    /// store bumped 1 → 2: this crate's assertion failed on a store that was working
    /// perfectly. A fresh in-memory database is migrated by the same code path a file
    /// database is, so its `user_version` *is* the store's constant. A future bump
    /// therefore cannot break these assertions, while a database that failed to migrate
    /// still can.
    fn store_schema_version() -> i32 {
        let fresh = Store::open_in_memory().expect("an in-memory store");
        fresh.migrate().expect("migrating a fresh store");
        fresh.schema_version().expect("reading its version")
    }

    #[test]
    fn the_clip_index_is_created_migrated_and_reopened_in_place() {
        let (dir, store) = index_dir();
        let db = dir.path().join("localplay.db");
        assert!(db.is_file(), "opening the index creates the database file");
        assert_eq!(
            store.schema_version().unwrap(),
            store_schema_version(),
            "opening the index migrates it to the store's own current schema version \
             (`localplay-store` keeps that number private; see `store_schema_version`)"
        );

        let path = dir.path().join("clip-1.mp4");
        std::fs::write(&path, vec![7u8; 500]).unwrap();
        index_clip(&store, &metadata(path, 500), 1_000).expect("the clip is indexed");
        drop(store);

        // A second start must adopt the same index — migration is forward-only and
        // idempotent — rather than start a fresh one over the top of it.
        let reopened = open_clip_index(&db).expect("reopen the index");
        assert_eq!(reopened.schema_version().unwrap(), store_schema_version());
        assert_eq!(reopened.list_clips().unwrap().len(), 1, "the clip survived the restart");

        // The capability the version gate exists for, asserted rather than assumed: the
        // session columns a Phase 5 engine writes into are present, so a session row can be
        // opened and closed on this index.
        let session = reopened
            .start_session(None, localplay_store::SESSION_MODE_BUFFER, 1_000, "scratch", 0)
            .expect("the index carries the session columns");
        reopened.end_session(session, 2_000, None, 0, 0).expect("and can close a session");
        assert_eq!(reopened.list_sessions().unwrap().len(), 1);
    }

    #[test]
    fn indexing_a_clip_records_the_values_it_was_handed() {
        let (dir, store) = index_dir();
        let path = dir.path().join("clip-1.mp4");
        std::fs::write(&path, vec![7u8; 500]).unwrap();
        let meta = metadata(path.clone(), std::fs::metadata(&path).unwrap().len());
        let before = now_ms();

        let id = index_clip(&store, &meta, 42_000).expect("the clip is indexed");
        let after = now_ms();

        let rows = store.list_clips().unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.id, id);
        assert_eq!(row.path, path);
        assert_eq!(row.started_at_ms, 42_000, "the ledger position is recorded as given");
        assert_eq!(row.duration_ms, meta.duration_ms);
        assert_eq!(
            row.size_bytes,
            std::fs::metadata(&path).unwrap().len(),
            "the recorded size is the real file's"
        );
        assert_eq!(
            row.codec, "h264_nvenc",
            "the encoder that produced the file, not the config's abstract codec"
        );
        assert!(!row.favourite, "a new clip is not a favourite");
        assert!(
            row.created_at_ms >= before && row.created_at_ms <= after,
            "created_at is stamped with the wall clock at insert ({})",
            row.created_at_ms
        );
        assert_eq!(store.total_bytes().unwrap(), 500);
    }

    #[test]
    fn a_clip_that_cannot_be_indexed_is_reported_and_the_file_is_kept() {
        let (dir, store) = index_dir();
        let path = dir.path().join("clip-1.mp4");
        std::fs::write(&path, vec![7u8; 500]).unwrap();
        let meta = metadata(path.clone(), 500);

        assert!(index_clip(&store, &meta, 1).is_some());
        // `clips.path` is UNIQUE, so indexing the same file twice fails — the index write
        // is the one thing that can fail after the clip exists.
        assert_eq!(index_clip(&store, &meta, 2), None, "a duplicate insert fails");
        assert!(
            path.is_file(),
            "a failed index write must never cost the clip the user asked for"
        );
        assert_eq!(store.list_clips().unwrap().len(), 1, "and must not corrupt the index");
    }

    #[test]
    fn indexing_an_event_records_the_reason_and_its_clip() {
        let (dir, store) = index_dir();
        let path = dir.path().join("clip-1.mp4");
        std::fs::write(&path, vec![7u8; 500]).unwrap();
        let clip_id = index_clip(&store, &metadata(path, 500), 30_000).expect("the clip is indexed");

        let kill = localplay_events::GameEvent::new(
            localplay_events::Source::Lol,
            localplay_events::EventKind::Kill,
            // One line of JSON, which is what the payload column holds.
            serde_json::json!({ "source": "lol", "killer": "Ahri" }),
        );
        let id = index_event(&store, &kill, 34_000, Some(clip_id), None).expect("the reason is written");

        let events = store.list_events().expect("read the events back");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, id);
        assert_eq!(events[0].kind, "kill", "the tag the vocabulary defines");
        assert_eq!(events[0].at_ms, 34_000, "media time, as given");
        assert_eq!(events[0].clip_id, Some(clip_id), "linked to the clip it produced");
        assert_eq!(events[0].session_id, None);
        assert!(events[0].payload.as_deref().unwrap_or("").contains("Ahri"));

        // A marker: recorded, no clip.
        let marker = localplay_events::GameEvent::bare(
            localplay_events::Source::Gsi,
            localplay_events::EventKind::RoundStart,
        );
        index_event(&store, &marker, 40_000, None, None).expect("a marker is written too");
        let events = store.list_events().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].kind, "round_start");
        assert_eq!(events[1].clip_id, None);
        assert_eq!(events[1].payload, None, "a bare event has no detail to record");
    }

    #[test]
    fn an_event_for_a_clip_that_does_not_exist_is_reported_rather_than_written() {
        // The foreign key is the store's; all this checks is that the failure comes back as
        // an error for the caller to decide about instead of being silently dropped.
        let (_dir, store) = index_dir();
        let event = localplay_events::GameEvent::bare(
            localplay_events::Source::Lol,
            localplay_events::EventKind::Kill,
        );
        assert!(index_event(&store, &event, 1_000, Some(7), None).is_err());
        assert!(store.list_events().unwrap().is_empty());
    }

    /// The storage policy, in the shape the config file gives it.
    fn storage(clips_dir: &Path, clips_cap: u64, sessions_dir: &Path, sessions_cap: u64) -> StorageSection {
        StorageSection {
            clips_dir: clips_dir.display().to_string(),
            max_total_bytes: clips_cap,
            max_age_days: 3_650,
            sessions: crate::config::SessionStorageRules {
                sessions_dir: sessions_dir.display().to_string(),
                max_total_bytes: sessions_cap,
                max_age_days: 3_650,
            },
        }
    }

    #[test]
    fn a_cleanup_pass_applies_the_configured_policy_through_the_index() {
        let (dir, store) = index_dir();
        let clips_dir = dir.path().join("clips");
        std::fs::create_dir_all(&clips_dir).unwrap();

        // Two clips of 1_000 bytes each, indexed exactly as the trigger path indexes them.
        let mut paths = Vec::new();
        for i in 1..=2u64 {
            let path = clips_dir.join(format!("clip-{i}.mp4"));
            std::fs::write(&path, vec![0u8; 1_000]).unwrap();
            index_clip(&store, &metadata(path.clone(), 1_000), i * 1_000).unwrap();
            paths.push(path);
        }

        // Room for one of the two clips, and a session store with nothing in it.
        let storage = storage(&clips_dir, 1_000, &dir.path().join("sessions"), 1 << 30);
        let mut report = CleanupReport::default();
        let outcome = cleanup_pass(&store, &storage, &mut report);

        let rows = store.list_clips().unwrap();
        assert_eq!(rows.len(), 1, "the cap allows one of the two clips");
        let survivor = rows[0].path.clone();
        assert!(survivor.is_file(), "the survivor's file is untouched");
        let evicted = paths.into_iter().find(|p| *p != survivor).expect("one was evicted");
        assert!(!evicted.exists(), "the evicted clip's row and its file are both gone");
        assert_eq!(store.total_bytes().unwrap(), 1_000);
        assert_eq!(outcome.clips.deleted, 1);
        assert_eq!(outcome.clips.bytes_reclaimed, 1_000, "measured from the files, not the rows");
        assert!(outcome.sessions.deleted == 0 && outcome.sessions.bytes_reclaimed == 0);
        assert!(outcome.clips.cap_met, "the clips cap is met after the pass");

        // A second pass has nothing to do, and must leave the survivor alone: this is the
        // pass that runs every `CLEANUP_INTERVAL` in the capture loop, so it has to be a
        // no-op once the library fits.
        let outcome = cleanup_pass(&store, &storage, &mut report);
        assert_eq!(store.list_clips().unwrap().len(), 1);
        assert!(survivor.is_file(), "an idle pass deletes nothing");
        assert_eq!(outcome.deleted(), 0);
    }

    /// The two libraries are evicted **independently** — a full session store must not
    /// push clips out, and vice versa — and a favourite survives in each of them.
    #[test]
    fn retention_evicts_clips_and_sessions_independently_with_favourites_immune() {
        let (dir, store) = index_dir();
        let clips_dir = dir.path().join("clips");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&clips_dir).unwrap();

        // Two clips, the newer one favourited.
        let mut clip_paths = Vec::new();
        for i in 1..=2u64 {
            let path = clips_dir.join(format!("clip-{i}.mp4"));
            std::fs::write(&path, vec![0u8; 1_000]).unwrap();
            let id = index_clip(&store, &metadata(path.clone(), 1_000), i * 1_000).unwrap();
            if i == 2 {
                store.set_favourite(id, true).unwrap();
            }
            clip_paths.push(path);
        }

        // Two finished sessions with their own directories and files, the newer favourited.
        let mut session_paths = Vec::new();
        for i in 1..=2i64 {
            let scratch = sessions_dir.join(format!("session-{i}"));
            std::fs::create_dir_all(&scratch).unwrap();
            std::fs::write(scratch.join("seg-000000.mp4"), vec![0u8; 400]).unwrap();
            let final_path = sessions_dir.join(format!("session-{i}.mp4"));
            std::fs::write(&final_path, vec![0u8; 600]).unwrap();
            let id = store
                .start_session(Some("Dota 2"), localplay_store::SESSION_MODE_SESSION, i * 1_000, &scratch.display().to_string(), 0)
                .unwrap();
            store.end_session(id, i * 1_000 + 5, Some(&final_path.display().to_string()), 1_000, 0).unwrap();
            if i == 2 {
                store.set_session_favourite(id, true).unwrap();
            }
            session_paths.push((final_path, scratch));
        }

        // Each cap allows exactly one (the favourite) and nothing more.
        let storage = storage(&clips_dir, 1_000, &sessions_dir, 1_000);
        let mut report = CleanupReport::default();
        let outcome = cleanup_pass(&store, &storage, &mut report);

        assert_eq!(store.list_clips().unwrap().len(), 1, "one clip survives");
        assert!(store.list_clips().unwrap()[0].favourite, "and it is the favourite");
        assert!(clip_paths[1].is_file(), "the favourite's file is untouched");
        assert!(!clip_paths[0].exists(), "the non-favourite clip is gone");

        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1, "one session survives");
        assert!(sessions[0].favourite, "and it is the favourite");
        assert!(session_paths[1].0.is_file(), "the favourite session's file is untouched");
        assert!(session_paths[1].1.is_dir(), "and so is its scratch directory");
        assert!(!session_paths[0].0.exists(), "the evicted session's file is gone");
        assert!(!session_paths[0].1.exists(), "and so is the directory its segments lived in");
        assert_eq!(store.total_session_bytes().unwrap(), 1_000);

        assert_eq!(outcome.clips.deleted, 1);
        assert_eq!(outcome.sessions.deleted, 1, "the pass reports both classes");
        assert_eq!(outcome.clips.bytes_reclaimed, 1_000);
        assert_eq!(outcome.sessions.bytes_reclaimed, 1_000);
        assert!(outcome.cap_met(), "both caps are met after the pass");
    }

    /// A cap that nothing may satisfy is reported, per class, and nothing is deleted to try.
    ///
    /// Both causes are real: a favourited session (immune by choice) and a session that is
    /// **still recording** (immune because its scratch directory is footage that exists
    /// nowhere else yet).
    #[test]
    fn an_unsatisfiable_cap_is_reported_for_each_class() {
        let (dir, store) = index_dir();
        let clips_dir = dir.path().join("clips");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&clips_dir).unwrap();
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // One favourited clip of 1_000 bytes against a 100-byte cap.
        let clip_path = clips_dir.join("clip-1.mp4");
        std::fs::write(&clip_path, vec![0u8; 1_000]).unwrap();
        let clip_id = index_clip(&store, &metadata(clip_path.clone(), 1_000), 1_000).unwrap();
        store.set_favourite(clip_id, true).unwrap();

        // A running session of 2_000 bytes against a 100-byte cap.
        let scratch = sessions_dir.join("session-1");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("seg-000000.mp4"), vec![0u8; 2_000]).unwrap();
        let running = store
            .start_session(None, localplay_store::SESSION_MODE_SESSION, 1_000, &scratch.display().to_string(), 0)
            .unwrap();
        store.set_session_size(running, 2_000).unwrap();

        let storage = storage(&clips_dir, 100, &sessions_dir, 100);
        let mut report = CleanupReport::default();
        let outcome = cleanup_pass(&store, &storage, &mut report);

        assert_eq!(outcome.deleted(), 0, "nothing may be deleted to satisfy these caps");
        assert!(!outcome.clips.cap_met && !outcome.sessions.cap_met);
        assert_eq!(report.last_clips_shortfall, Some(900), "the favourite's excess is reported");
        assert_eq!(report.last_sessions_shortfall, Some(1_900), "the live session's is too");
        assert!(clip_path.is_file(), "the favourite is untouched");
        assert!(scratch.is_dir(), "and the live session's footage is not touched");
        assert!(store.get_session(running).unwrap().unwrap().ended_at_ms.is_none());

        // The report is remembered: a second identical pass does not change it (it is the
        // same shortfall), and once the immunity is gone the next pass evicts both classes.
        let outcome = cleanup_pass(&store, &storage, &mut report);
        assert_eq!(outcome.deleted(), 0, "still nothing to do: {outcome:?}");
        assert_eq!(report.last_sessions_shortfall, Some(1_900));

        // The recording ends (with the bytes its directory holds) and the clip is
        // un-favourited: now the same policy has something it may delete.
        store.end_session(running, 9_000, None, 2_000, 0).unwrap();
        store.set_favourite(clip_id, false).unwrap();
        let outcome = cleanup_pass(&store, &storage, &mut report);
        assert_eq!(outcome.deleted(), 2, "once nothing is immune, both classes shrink: {outcome:?}");
        assert_eq!(outcome.clips.bytes_reclaimed, 1_000);
        assert_eq!(outcome.sessions.bytes_reclaimed, 2_000);
        assert!(outcome.cap_met(), "both caps are met after the pass: {outcome:?}");
        assert_eq!(report.last_clips_shortfall, None, "the unsatisfiable condition is gone");
        assert_eq!(report.last_sessions_shortfall, None);
        assert!(!clip_path.exists(), "the clip's file went with its row");
        assert!(!scratch.exists(), "and the session's scratch directory with its row");
    }
}
