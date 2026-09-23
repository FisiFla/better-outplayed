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
use localplay_store::cleanup::{execute_cleanup, plan_cleanup, CleanupPolicy};
use localplay_store::{NewClip, NewEvent, Store};
use std::path::Path;

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

/// Insert the `events` row for a derived game event (spec §5.5), linked to the clip it
/// produced when it produced one.
///
/// * `at_ms` is the event's position on the **ledger's media timeline** — the same clock
///   `clips.started_at` is on, so a session timeline can place a marker against a clip
///   without converting anything. For a clip's own event this is the *trigger* instant,
///   which is `buffer.pre_seconds` into the clip, because that is the moment the event
///   happened; the window merely starts earlier.
/// * `clip_id` is `None` for a marker (a game or round boundary, recorded but not clipped —
///   see `EventKind::is_highlight`) and for an event whose clip could not be indexed. The
///   column is nullable for exactly those cases.
/// * `session_id` is left NULL, as `index_clip` leaves it: this engine opens no `sessions`
///   row.
///
/// Returns the new row's id, or the error: the callers differ on what a failure means. A
/// clip that has already been written must not be lost to a bookkeeping failure, while a
/// marker is the *only* thing the caller asked for, so failing it is worth reporting.
pub fn index_event(store: &Store, event: &GameEvent, at_ms: u64, clip_id: Option<i64>) -> Result<i64> {
    let new = NewEvent {
        session_id: None,
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
#[derive(Default, Debug)]
pub struct CleanupReport {
    /// The shortfall reported by the last pass that could not meet the cap; `None` while
    /// the cap is met, so a recurrence warns again.
    last_shortfall: Option<u64>,
}

/// Apply the storage policy once (spec §8.1): plan against the index, execute the plan,
/// and report what happened.
///
/// The decision is [`plan_cleanup`]'s and the deletion ordering is
/// [`execute_cleanup`]'s; this function only supplies the policy from the config and the
/// clock, and decides what is worth a log line. It is silent when there is nothing to do,
/// because it runs at startup and then every [`crate::CLEANUP_INTERVAL`], and an idle pass
/// is not news.
pub fn cleanup_pass(store: &Store, storage: &StorageSection, report: &mut CleanupReport) {
    let policy = CleanupPolicy {
        max_total_bytes: storage.max_total_bytes,
        max_age_days: storage.max_age_days,
    };
    let clips = match store.list_clips() {
        Ok(clips) => clips,
        Err(err) => {
            tracing::warn!("storage policy: cannot read the clip index: {err:#}");
            return;
        }
    };
    let plan = plan_cleanup(&clips, &policy, now_ms());
    let outcome = match execute_cleanup(store, &plan) {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::warn!("storage policy: the pass could not be completed: {err:#}");
            return;
        }
    };

    if outcome.deleted > 0 {
        tracing::info!(
            "storage policy: deleted {} clip(s), reclaimed {} bytes; the clips directory \
             now holds {} bytes against a {} byte cap",
            outcome.deleted,
            outcome.bytes_reclaimed,
            outcome.bytes_after,
            policy.max_total_bytes
        );
    }
    if outcome.failed > 0 {
        tracing::warn!(
            "storage policy: {} planned deletion(s) could not be applied; the clips \
             directory is larger than the policy asked for",
            outcome.failed
        );
    }

    // A cap that the favourites alone exceed is not merely "not met this pass" — no pass
    // can meet it, and the spec forbids deleting anything else to try (spec §8.1). The
    // shortfall is what the user has to act on, so it is worth a warning, but only when
    // it appears or changes: the condition is permanent, and repeating it verbatim every
    // pass would bury everything else in the log.
    if outcome.cap_met {
        report.last_shortfall = None;
    } else if report.last_shortfall != Some(plan.over_cap_by_bytes) {
        tracing::warn!(
            "storage policy: storage.max_total_bytes ({}) cannot be satisfied — the \
             favourited clips alone exceed it by {} bytes, and favourites are exempt from \
             both rules, so nothing is deleted for it. The clips directory holds {} bytes. \
             Raise storage.max_total_bytes or un-favourite some clips.",
            policy.max_total_bytes,
            plan.over_cap_by_bytes,
            outcome.bytes_after
        );
        report.last_shortfall = Some(plan.over_cap_by_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The clip metadata `RingBuffer::trigger` hands back, without running ffmpeg.
    fn metadata(path: PathBuf, size_bytes: u64) -> ClipMetadata {
        ClipMetadata { path, duration_ms: 12_000, size_bytes, encoder: "h264_nvenc".into() }
    }

    /// An open index in a temp directory, plus the directory holding it.
    fn index_dir() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = open_clip_index(&dir.path().join("localplay.db")).expect("open the index");
        (dir, store)
    }

    #[test]
    fn the_clip_index_is_created_migrated_and_reopened_in_place() {
        let (dir, store) = index_dir();
        let db = dir.path().join("localplay.db");
        assert!(db.is_file(), "opening the index creates the database file");
        assert_eq!(store.schema_version().unwrap(), 1);

        let path = dir.path().join("clip-1.mp4");
        std::fs::write(&path, vec![7u8; 500]).unwrap();
        index_clip(&store, &metadata(path, 500), 1_000).expect("the clip is indexed");
        drop(store);

        // A second start must adopt the same index — migration is forward-only and
        // idempotent — rather than start a fresh one over the top of it.
        let reopened = open_clip_index(&db).expect("reopen the index");
        assert_eq!(reopened.schema_version().unwrap(), 1);
        assert_eq!(reopened.list_clips().unwrap().len(), 1, "the clip survived the restart");
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
        let id = index_event(&store, &kill, 34_000, Some(clip_id)).expect("the reason is written");

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
        index_event(&store, &marker, 40_000, None).expect("a marker is written too");
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
        assert!(index_event(&store, &event, 1_000, Some(7)).is_err());
        assert!(store.list_events().unwrap().is_empty());
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

        let storage = StorageSection {
            clips_dir: clips_dir.display().to_string(),
            max_total_bytes: 1_000, // room for one of the two clips
            max_age_days: 3_650,
        };
        let mut report = CleanupReport::default();
        cleanup_pass(&store, &storage, &mut report);

        let rows = store.list_clips().unwrap();
        assert_eq!(rows.len(), 1, "the cap allows one of the two clips");
        let survivor = rows[0].path.clone();
        assert!(survivor.is_file(), "the survivor's file is untouched");
        let evicted = paths.into_iter().find(|p| *p != survivor).expect("one was evicted");
        assert!(!evicted.exists(), "the evicted clip's row and its file are both gone");
        assert_eq!(store.total_bytes().unwrap(), 1_000);

        // A second pass has nothing to do, and must leave the survivor alone: this is the
        // pass that runs every `CLEANUP_INTERVAL` in the capture loop, so it has to be a
        // no-op once the library fits.
        cleanup_pass(&store, &storage, &mut report);
        assert_eq!(store.list_clips().unwrap().len(), 1);
        assert!(survivor.is_file(), "an idle pass deletes nothing");
    }
}
