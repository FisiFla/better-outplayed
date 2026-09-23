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
    pub fn delete_clip_returning_path(&self, id: i64) -> Result<Option<String>> {
        let path: Option<String> = self
            .conn
            .query_row("SELECT path FROM clips WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?;
        let Some(path) = path else {
            return Ok(None);
        };
        let removed = self.conn.execute("DELETE FROM clips WHERE id = ?1", params![id])?;
        Ok((removed == 1).then_some(path))
    }
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
