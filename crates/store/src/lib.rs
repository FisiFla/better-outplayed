//! SQLite index of clips and sessions. Schema mirrors spec §5.5.
//!
//! The storage policy (spec §8) is built on top of this: [`cleanup`] decides what may be
//! deleted from the rows this module hands out, and applies that decision with the
//! delete-before-unlink ordering the spec requires.

pub mod cleanup;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: i32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewClip {
    pub path: PathBuf,
    pub started_at_ms: u64,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub codec: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    pub id: i64,
    pub path: PathBuf,
    /// The clip's first frame on the encoder's **media** timeline, in ms — the ledger's
    /// own clock, not the wall clock (see the CLI's `index_clip`). It is not comparable
    /// across captures: it restarts with the scratch directory.
    pub started_at_ms: u64,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub codec: String,
    pub favourite: bool,
    /// Wall-clock instant this row was written, in ms since the Unix epoch
    /// (`clips.created_at`, stamped by [`Store::insert_clip`] — never by the caller).
    ///
    /// This is the only instant in the row that is comparable across runs, so it is the
    /// one the storage policy ages clips by and orders a library by (see
    /// [`cleanup::plan_cleanup`]).
    pub created_at_ms: i64,
}

/// A row to write into `events` (spec §5.5).
///
/// `at_ms` is on the **ledger's media timeline**, the same clock `clips.started_at` is on,
/// and not the wall clock. That is deliberate: the only thing anybody does with an event row
/// is place it against a clip (spec §9 joins event markers to clips by timestamp), and the
/// clip's own row is in media time. A wall-clock value here would be a number that cannot be
/// compared with the row it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    /// Always `None` for now: this engine opens no `sessions` row (see the recorder's
    /// `index_clip` for why). The column exists for the session recording of Phase 5.
    pub session_id: Option<i64>,
    /// `EventKind::as_tag`, e.g. `"kill"` or `"bomb_planted"`.
    pub kind: String,
    pub at_ms: u64,
    /// Integration-specific detail as one line of JSON (spec §5.5).
    pub payload: Option<String>,
    /// The clip this event produced, when it produced one. `None` is meaningful: a marker
    /// (a game or round starting) is recorded without a clip, and so is an event whose
    /// clip could not be indexed.
    pub clip_id: Option<i64>,
}

/// A row of `events`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: i64,
    pub session_id: Option<i64>,
    pub kind: String,
    pub at_ms: u64,
    pub payload: Option<String>,
    pub clip_id: Option<i64>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self { conn: Connection::open_in_memory().context("opening in-memory db")? })
    }

    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self { conn: Connection::open(path).with_context(|| format!("opening {}", path.display()))? })
    }

    /// Forward-only migrations. A schema newer than this build is refused rather
    /// than silently downgraded.
    pub fn migrate(&self) -> Result<()> {
        let current: i32 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current > SCHEMA_VERSION {
            anyhow::bail!(
                "database schema v{current} is newer than this build supports (v{SCHEMA_VERSION}); \
                 refusing to downgrade"
            );
        }
        if current == SCHEMA_VERSION {
            return Ok(());
        }

        self.conn.execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS sessions (
                id          INTEGER PRIMARY KEY,
                game        TEXT,
                started_at  INTEGER NOT NULL,
                ended_at    INTEGER,
                scratch_dir TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS clips (
                id          INTEGER PRIMARY KEY,
                session_id  INTEGER REFERENCES sessions(id),
                path        TEXT NOT NULL UNIQUE,
                started_at  INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                size_bytes  INTEGER NOT NULL,
                codec       TEXT NOT NULL,
                favourite   INTEGER NOT NULL DEFAULT 0,
                created_at  INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS events (
                id         INTEGER PRIMARY KEY,
                session_id INTEGER REFERENCES sessions(id),
                kind       TEXT NOT NULL,
                at         INTEGER NOT NULL,
                payload    TEXT,
                clip_id    INTEGER REFERENCES clips(id)
            );
            CREATE INDEX IF NOT EXISTS idx_clips_started_at ON clips(started_at);
            CREATE INDEX IF NOT EXISTS idx_events_session   ON events(session_id, at);
            "#,
        )?;
        self.conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i32> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    pub fn insert_clip(&self, clip: &NewClip) -> Result<i64> {
        let path = clip.path.to_string_lossy();
        self.conn.execute(
            "INSERT INTO clips (path, started_at, duration_ms, size_bytes, codec, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                path,
                clip.started_at_ms as i64,
                clip.duration_ms as i64,
                clip.size_bytes as i64,
                clip.codec,
                now_ms(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn list_clips(&self) -> Result<Vec<Clip>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, started_at, duration_ms, size_bytes, codec, favourite, created_at
             FROM clips ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Clip {
                id: r.get(0)?,
                path: PathBuf::from(r.get::<_, String>(1)?),
                started_at_ms: r.get::<_, i64>(2)? as u64,
                duration_ms: r.get::<_, i64>(3)? as u64,
                size_bytes: r.get::<_, i64>(4)? as u64,
                codec: r.get(5)?,
                favourite: r.get::<_, i64>(6)? != 0,
                created_at_ms: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Write one `events` row and return its id.
    ///
    /// The foreign key on `clip_id` is enforced (the migration turns `foreign_keys` on for
    /// this connection), so an event cannot be attached to a clip that does not exist: a
    /// marker pointing at nothing would be worse than no marker.
    pub fn insert_event(&self, event: &NewEvent) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO events (session_id, kind, at, payload, clip_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event.session_id,
                event.kind,
                event.at_ms as i64,
                event.payload,
                event.clip_id,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Every event, oldest first on the media timeline — the order a session timeline
    /// renders them in.
    pub fn list_events(&self) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, kind, at, payload, clip_id FROM events ORDER BY at, id",
        )?;
        let rows = stmt.query_map([], read_event)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The events attached to one clip, oldest first. This is what a clip's detail pane
    /// shows beside the footage.
    pub fn events_for_clip(&self, clip_id: i64) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, kind, at, payload, clip_id FROM events
             WHERE clip_id = ?1 ORDER BY at, id",
        )?;
        let rows = stmt.query_map(params![clip_id], read_event)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Mark a clip as a favourite, or clear it. Favourites are exempt from both storage
    /// rules (spec §8.1), so this is the user's only way to protect a clip from the
    /// manager.
    ///
    /// Returns whether a row was updated: `false` means no clip with that id exists,
    /// which the caller may want to report rather than assume.
    pub fn set_favourite(&self, id: i64, favourite: bool) -> Result<bool> {
        let updated = self.conn.execute(
            "UPDATE clips SET favourite = ?2 WHERE id = ?1",
            params![id, favourite as i64],
        )?;
        Ok(updated > 0)
    }

    /// Total bytes the indexed clips claim — the figure `storage.max_total_bytes` caps
    /// (spec §8.1). It is what the rows recorded when they were written, not a fresh
    /// `stat` of the files, so a file changed behind the index's back will disagree.
    pub fn total_bytes(&self) -> Result<u64> {
        let bytes: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(size_bytes), 0) FROM clips",
            [],
            |r| r.get(0),
        )?;
        Ok(bytes.max(0) as u64)
    }

    /// Delete the row and return its path so the caller can unlink *afterwards*
    /// (spec §8.2). Returns `None` if this call did not remove a row.
    ///
    /// The path is handed back only when this call's own `DELETE` removed the row — a
    /// row that disappeared between the read and the delete (another cleanup pass, a
    /// second instance, a manual `sqlite3` session) yields `None`. That is what makes the
    /// returned path a path a *committed* delete removed: the caller may unlink it.
    ///
    /// **Events that pointed at the clip stay, and lose the link.** `events.clip_id` is a
    /// foreign key, so deleting a clip an event names would otherwise fail — and the storage
    /// policy would then be unable to evict exactly the clips a user has clipped most
    /// (`FOREIGN KEY constraint failed`, on the first clip that has an event). The rows that
    /// record *what happened* are history, not footage: the manager's business is the clips
    /// directory, so the clip goes and its events are kept with `clip_id` set to NULL — the
    /// same state a marker (a game start, a round end) has. Both writes are one transaction,
    /// so a crash cannot leave events detached from a clip that still exists.
    pub fn delete_clip_returning_path(&self, id: i64) -> Result<Option<String>> {
        let path: Option<String> = self
            .conn
            .query_row("SELECT path FROM clips WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?;
        let Some(path) = path else {
            return Ok(None);
        };
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("UPDATE events SET clip_id = NULL WHERE clip_id = ?1", params![id])?;
        let removed = tx.execute("DELETE FROM clips WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok((removed == 1).then_some(path))
    }
}

fn read_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        id: row.get(0)?,
        session_id: row.get(1)?,
        kind: row.get(2)?,
        at_ms: row.get::<_, i64>(3)? as u64,
        payload: row.get(4)?,
        clip_id: row.get(5)?,
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrating_is_idempotent() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let v1 = s.schema_version().unwrap();
        s.migrate().unwrap();
        assert_eq!(s.schema_version().unwrap(), v1, "re-migrating must not bump the version");
    }

    #[test]
    fn inserts_and_lists_clips_newest_first() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let a = s.insert_clip(&NewClip {
            path: "/clips/a.mp4".into(),
            started_at_ms: 1_000,
            duration_ms: 4_000,
            size_bytes: 100,
            codec: "h264".into(),
        }).unwrap();
        let _b = s.insert_clip(&NewClip {
            path: "/clips/b.mp4".into(),
            started_at_ms: 2_000,
            duration_ms: 4_000,
            size_bytes: 200,
            codec: "h264".into(),
        }).unwrap();

        let clips = s.list_clips().unwrap();
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].path, PathBuf::from("/clips/b.mp4"), "newest first");
        assert_eq!(clips[1].id, a);
    }

    #[test]
    fn rejects_a_duplicate_clip_path() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let clip = NewClip {
            path: "/clips/dup.mp4".into(),
            started_at_ms: 1_000,
            duration_ms: 1_000,
            size_bytes: 1,
            codec: "h264".into(),
        };
        s.insert_clip(&clip).unwrap();
        assert!(s.insert_clip(&clip).is_err(), "path is UNIQUE");
    }

    #[test]
    fn records_the_creation_time_of_a_clip() {
        // The storage policy's age rule ages against `clips.created_at`, which the store
        // stamps itself: a caller cannot backdate a clip into (or out of) deletion.
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let before = now_ms();
        let id = s.insert_clip(&NewClip {
            path: "/clips/timed.mp4".into(),
            started_at_ms: 1_000,
            duration_ms: 1_000,
            size_bytes: 1,
            codec: "h264".into(),
        }).unwrap();
        let after = now_ms();

        let clip = s.list_clips().unwrap().into_iter().find(|c| c.id == id).unwrap();
        assert!(clip.created_at_ms >= before && clip.created_at_ms <= after, "got {}", clip.created_at_ms);
    }

    #[test]
    fn toggles_a_favourite_and_reports_a_missing_id() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let id = s.insert_clip(&NewClip {
            path: "/clips/fav.mp4".into(),
            started_at_ms: 1_000,
            duration_ms: 1_000,
            size_bytes: 1,
            codec: "h264".into(),
        }).unwrap();
        assert!(!s.list_clips().unwrap()[0].favourite, "clips are not favourites by default");

        assert!(s.set_favourite(id, true).unwrap());
        assert!(s.list_clips().unwrap()[0].favourite);
        assert!(s.set_favourite(id, false).unwrap());
        assert!(!s.list_clips().unwrap()[0].favourite);

        assert!(
            !s.set_favourite(id + 1, true).unwrap(),
            "no such row: the caller must be able to tell a no-op from an update"
        );
    }

    #[test]
    fn sums_the_indexed_bytes() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        assert_eq!(s.total_bytes().unwrap(), 0, "an empty index holds nothing");

        for (path, bytes) in [("/clips/a.mp4", 100u64), ("/clips/b.mp4", 250)] {
            s.insert_clip(&NewClip {
                path: path.into(),
                started_at_ms: 1_000,
                duration_ms: 1_000,
                size_bytes: bytes,
                codec: "h264".into(),
            }).unwrap();
        }
        assert_eq!(s.total_bytes().unwrap(), 350);
    }

    fn an_event(kind: &str, at_ms: u64, clip_id: Option<i64>) -> NewEvent {
        NewEvent {
            session_id: None,
            kind: kind.to_string(),
            at_ms,
            payload: Some(format!("{{\"source\":\"gsi\",\"event\":\"{kind}\"}}")),
            clip_id,
        }
    }

    #[test]
    fn records_an_event_with_and_without_a_clip() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let clip = s
            .insert_clip(&NewClip {
                path: "/clips/a.mp4".into(),
                started_at_ms: 30_000,
                duration_ms: 35_000,
                size_bytes: 100,
                codec: "h264_nvenc".into(),
            })
            .unwrap();

        // A highlight: the event that produced the clip.
        let kill = s.insert_event(&an_event("kill", 34_000, Some(clip))).unwrap();
        // A marker: recorded, no clip.
        let round = s.insert_event(&an_event("round_start", 40_000, None)).unwrap();
        assert_ne!(kill, round);

        let events = s.list_events().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "kill", "oldest first on the media timeline");
        assert_eq!(events[0].at_ms, 34_000);
        assert_eq!(events[0].clip_id, Some(clip));
        assert!(events[0].payload.as_deref().unwrap().contains("kill"));
        assert_eq!(events[0].session_id, None, "this engine opens no session row");
        assert_eq!(events[1].kind, "round_start");
        assert_eq!(events[1].clip_id, None, "a marker has no clip");

        let for_clip = s.events_for_clip(clip).unwrap();
        assert_eq!(for_clip.len(), 1, "the clip's own events");
        assert_eq!(for_clip[0].id, kill);
        assert!(s.events_for_clip(clip + 1).unwrap().is_empty(), "no such clip, no events");
    }

    #[test]
    fn an_event_cannot_point_at_a_clip_that_does_not_exist() {
        // `events.clip_id` is a foreign key and the migration turns enforcement on, so a
        // marker cannot be attached to a clip that was never written.
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        assert!(
            s.insert_event(&an_event("kill", 1_000, Some(42))).is_err(),
            "there is no clip 42"
        );
        assert!(s.list_events().unwrap().is_empty(), "and nothing was written");
    }

    #[test]
    fn an_event_with_no_payload_is_a_row_like_any_other() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        s.insert_event(&NewEvent {
            session_id: None,
            kind: "game_start".into(),
            at_ms: 0,
            payload: None,
            clip_id: None,
        })
        .unwrap();
        let events = s.list_events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload, None, "the column is nullable and stays null");
    }

    #[test]
    fn deleting_a_clip_releases_its_events_instead_of_failing() {
        // The interaction that would have made the storage policy unusable the moment an
        // integration was switched on: `events.clip_id` is a foreign key, so evicting a clip
        // an event names has to detach the event rather than be refused.
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let clip = s
            .insert_clip(&NewClip {
                path: "/clips/gone.mp4".into(),
                started_at_ms: 1_000,
                duration_ms: 1_000,
                size_bytes: 10,
                codec: "h264".into(),
            })
            .unwrap();
        s.insert_event(&an_event("kill", 1_000, Some(clip))).unwrap();

        // Delete the clip the way the executor does: row first, then the file (there is no
        // file here, which the executor tolerates).
        assert_eq!(s.delete_clip_returning_path(clip).unwrap().as_deref(), Some("/clips/gone.mp4"));
        let events = s.list_events().unwrap();
        assert_eq!(events.len(), 1, "the event row survives the clip it pointed at");
        assert_eq!(events[0].clip_id, None, "and no longer names a row that is gone");
        assert!(s.list_clips().unwrap().is_empty());
    }

    #[test]
    fn delete_before_unlink_ordering_is_exposed_to_the_caller() {
        // The store must be able to delete the row and hand back the path, so the
        // caller can unlink afterwards. Spec §8.2: no file is deleted without a
        // committed row naming it.
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let id = s.insert_clip(&NewClip {
            path: "/clips/c.mp4".into(),
            started_at_ms: 1_000,
            duration_ms: 1_000,
            size_bytes: 1,
            codec: "h264".into(),
        }).unwrap();

        let removed = s.delete_clip_returning_path(id).unwrap();
        assert_eq!(removed.as_deref(), Some("/clips/c.mp4"));
        assert_eq!(s.delete_clip_returning_path(id).unwrap(), None, "second delete is a no-op");
    }
}
