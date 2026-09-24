//! SQLite index of clips and sessions. Schema mirrors spec §5.5.
//!
//! The storage policy (spec §8) is built on top of this: [`cleanup`] decides what may be
//! deleted from the rows this module hands out, and applies that decision with the
//! delete-before-unlink ordering the spec requires.

pub mod cleanup;
pub mod retention;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

/// The schema this build writes. Each version adds columns in place, without reading,
/// copying or losing a row (see [`Store::migrate`]): **v2** the session columns Phase 5
/// needs, **v3** the media-clock anchor a session's event timeline is measured from.
const SCHEMA_VERSION: i32 = 3;

/// The mode of a session the ring buffer opened: the row exists to group the clips and
/// events of one buffer run, and there is no concatenated session file (spec §5.5).
pub const SESSION_MODE_BUFFER: &str = "buffer";

/// The mode of a session the full-session recorder opened: the scratch segments are
/// concatenated into one file, which `sessions.final_path` names (spec §5.5).
pub const SESSION_MODE_SESSION: &str = "session";

/// Every mode [`Store::start_session`] accepts. A closed set, because every reader of
/// `sessions.mode` switches on it.
const SESSION_MODES: [&str; 2] = [SESSION_MODE_BUFFER, SESSION_MODE_SESSION];

/// The v1 tables, exactly as spec §5.5 defines them.
///
/// Created `IF NOT EXISTS` on every upgrade path, so this is both "the schema of a new
/// database" and "the base a v1 database already has" — one definition, and the v1 → v2
/// step below is what carries a database from it to the current shape.
const V1_DDL: &str = r#"
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
            "#;

/// The columns schema v2 adds to `sessions`, and the DDL that adds each.
///
/// Each one is `ADD COLUMN`-able in place: SQLite can add a column with a *constant*
/// default without rebuilding the table, so no row is read, copied or lost.
///
/// # Why a legacy session's `mode` defaults to `'buffer'`
///
/// A `sessions` row that exists before this build was written by the buffer engine or not
/// at all: the full-session recorder this column exists for has never run in a released
/// build, and `'session'` would assert a concatenated session file the row demonstrably
/// does not have (`final_path` is NULL and `size_bytes` is 0, which is a different default
/// in the same change). So `'buffer'` is the only claim a legacy row supports — it
/// describes the row's own contents rather than inventing data. Anything else would make
/// the session store look like it held artefacts it never held, and would charge that
/// phantom to a retention cap.
const V2_SESSION_COLUMNS: [(&str, &str); 4] = [
    (
        "mode",
        "ALTER TABLE sessions ADD COLUMN mode TEXT NOT NULL DEFAULT 'buffer'",
    ),
    ("final_path", "ALTER TABLE sessions ADD COLUMN final_path TEXT"),
    (
        "size_bytes",
        "ALTER TABLE sessions ADD COLUMN size_bytes INTEGER NOT NULL DEFAULT 0",
    ),
    (
        "favourite",
        "ALTER TABLE sessions ADD COLUMN favourite INTEGER NOT NULL DEFAULT 0",
    ),
];

/// The columns schema v3 adds to `sessions`, and the DDL that adds each.
///
/// One column, and it fixes a real defect rather than tidying one. `events.at` is **media**
/// time — the ledger's clock, which restarts with the scratch directory — while
/// `started_at` is the **wall** clock. The session timeline used to be their difference,
/// which is arithmetic on two unrelated clocks: it is why a session's events plotted
/// nowhere near the moment they happened. `media_epoch_ms` records the media position the
/// session began at, so the timeline becomes `at - media_epoch_ms` — one clock, and the
/// number the scrubber plots.
///
/// A legacy row defaults to 0, and that is honest rather than convenient: those rows were
/// recorded against the buffer's own ledger, and 0 is the only epoch this build can supply
/// for them without inventing a measurement it never took. It is the same choice v2 made
/// for `mode`, and for the same reason — the default describes what the row's own contents
/// support.
const V3_SESSION_COLUMNS: [(&str, &str); 1] = [(
    "media_epoch_ms",
    "ALTER TABLE sessions ADD COLUMN media_epoch_ms INTEGER NOT NULL DEFAULT 0",
)];

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

/// A row of `sessions` (spec §5.5): one recording session, and everything the session
/// manager and the review UI need to show and manage it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: i64,
    pub game: Option<String>,
    /// [`SESSION_MODE_BUFFER`] or [`SESSION_MODE_SESSION`]; a closed set today, stored as
    /// TEXT so a row written by a newer build is still readable rather than an error.
    pub mode: String,
    /// When the session started on the **wall** clock — the instant the retention rules age
    /// a session by and order the session store with (see [`retention::plan_retention`]),
    /// and what a UI shows as "recorded at". It is deliberately *not* what the event
    /// timeline is measured from: `events.at` is media time, and subtracting one from the
    /// other was arithmetic across two unrelated clocks. That is
    /// [`Session::media_epoch_ms`]'s job.
    pub started_at_ms: i64,
    /// The **media** position this session began at, in ms: what
    /// [`Store::events_for_session`] subtracts from `events.at` to place an event on the
    /// session timeline. `0` for a session that started against a scratch directory whose
    /// ledger was empty — the ordinary case — and for every row written before schema v3.
    ///
    /// Two clocks, two columns, on purpose. Media time restarts with the scratch directory
    /// and is the axis a clip's frames are measured on; wall time survives a restart and is
    /// the axis "how old is this" is measured on. A session that reuses a scratch directory
    /// inherits a non-zero epoch, so a single column cannot serve both purposes, and
    /// subtracting the wrong one put every event of such a session in the wrong place.
    pub media_epoch_ms: i64,
    /// When the recording stopped. **`None` means it is still recording** — the one state
    /// in the row that is not history, and the one retention must never evict.
    pub ended_at_ms: Option<i64>,
    /// The directory the session's segments are written into, and the one the manager
    /// removes when the session is deleted. `NOT NULL` in the schema, and it is not wrong
    /// for a buffer-mode session: the ring writes segments into a scratch directory just
    /// as the session recorder does.
    pub scratch_dir: String,
    /// The concatenated session file, once the session has been finalised. `None` until
    /// [`Store::end_session`] is told about one — a buffer-mode session never has one.
    pub final_path: Option<String>,
    /// Bytes the session occupies, as recorded when it was finalised. Written by
    /// [`Store::end_session`] (and, while it is still running, by
    /// [`Store::set_session_size`]); the retention cap's arithmetic uses this number.
    pub size_bytes: i64,
    /// Exempt from both retention rules, exactly as a favourited clip is
    /// (spec §8.1). [`Store::set_session_favourite`] is the user's only way in.
    pub favourite: bool,
}

/// One row of the session timeline: an event, placed against the session that was
/// recording when it happened.
///
/// The UI colours these by `kind` and plots them by `offset_ms`, so nothing here is
/// filtered: a manual hotkey bookmark and a game-derived kill or death are all rows of
/// the same timeline, and which of them *produced a clip* is [`SessionEvent::clip_id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEvent {
    pub id: i64,
    /// `events.kind` — the integration's tag (`"kill"`, `"death"`, a hotkey bookmark),
    /// passed through verbatim. `localplay-store` does not know the vocabulary and must
    /// not: a tag this build has never heard of is a row that exists, not an error.
    pub kind: String,
    /// The event's position on the session timeline: `events.at - sessions.media_epoch_ms`,
    /// in ms. Derived in SQL, in exactly one place ([`Store::events_for_session`]), so
    /// the scrubber, a test and the UI cannot disagree about it. `0` is an event at the
    /// very instant the session started; a **negative** offset is possible and is
    /// reported rather than clamped — it means a row was written before the session's own
    /// start instant, which a caller needs to see, not to have rounded to zero.
    pub offset_ms: i64,
    /// Integration-specific detail as one line of JSON, when there is any.
    pub payload: Option<String>,
    /// The clip this event produced, when it produced one. `None` for a marker — a
    /// bookmark that was never clipped, a round boundary, or an event whose clip could
    /// not be indexed.
    pub clip_id: Option<i64>,
}

/// What a deleted session row leaves for the caller to remove from disk, in the order the
/// deletion ordering (spec §8.2) implies: the row is already committed gone, and these
/// are the paths it named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPaths {
    /// The concatenated session file, when the session had one.
    pub final_path: Option<String>,
    /// The session's scratch directory — its segments, possibly still growing if the
    /// session never ended cleanly (see [`Store::delete_session_returning_paths`]).
    pub scratch_dir: String,
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
    ///
    /// Every version is reached through the same path, so a database this build creates
    /// and a v1 database it upgrades end up with the same shape: the v1 tables are
    /// created `IF NOT EXISTS` (a no-op on a database that already has them), then each
    /// step the database has not had yet is applied in order, then the version is
    /// stamped. `PRAGMA user_version` is written last, so a crash part-way leaves a
    /// database that is one step behind and is upgraded by the next call — never a
    /// database claiming a version it does not have.
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

        self.conn.execute_batch(V1_DDL)?;
        if current < 2 {
            self.upgrade_to_v2()?;
        }
        if current < 3 {
            self.upgrade_to_v3()?;
        }
        self.conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        Ok(())
    }

    /// v1 → v2: `sessions` gains the columns the session recorder needs.
    fn upgrade_to_v2(&self) -> Result<()> {
        self.add_session_columns(&V2_SESSION_COLUMNS)
    }

    /// v2 → v3: `sessions` gains the media-clock anchor its event timeline is measured
    /// from (see [`V3_SESSION_COLUMNS`]).
    fn upgrade_to_v3(&self) -> Result<()> {
        self.add_session_columns(&V3_SESSION_COLUMNS)
    }

    /// Add whichever of `columns` the `sessions` table does not already have.
    ///
    /// Safe to run against a database it has already been applied to, on purpose and twice
    /// over. The version check in [`Store::migrate`] means it normally is not; and each
    /// column is checked against `PRAGMA table_info` before it is added, so a database that
    /// already has one of them — a hand-edited file, an index another build half-upgraded —
    /// upgrades instead of dying on `duplicate column name`. The whole set is one
    /// transaction, so an interrupted upgrade leaves the table as it was rather than
    /// half-widened.
    fn add_session_columns(&self, columns: &[(&str, &str)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (column, ddl) in columns {
            if !column_exists(&tx, "sessions", column)? {
                tx.execute_batch(ddl)?;
            }
        }
        tx.commit()?;
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

    // ---------------------------------------------------------------- sessions

    /// Open a `sessions` row and return its id (spec §5.5). The session is *running* from
    /// this moment: `ended_at` is NULL until [`Store::end_session`] is told otherwise, and
    /// that NULL is what the retention rules read as "still recording" (see
    /// [`retention::plan_retention`]).
    ///
    /// `mode` is which engine opened it — [`SESSION_MODE_BUFFER`] or
    /// [`SESSION_MODE_SESSION`] — and is **validated**: an unknown mode is refused rather
    /// than stored, because every reader of the column switches on it and a typo would
    /// quietly read as "not the mode I know". The set is closed until a third engine
    /// exists, at which point it is one line here and one arm in the readers.
    ///
    /// `started_at_ms` is when it started on the **wall** clock and `media_epoch_ms` is
    /// where it started on the **media** clock — two clocks, both recorded, neither
    /// derivable from the other. The event timeline is `events.at - media_epoch_ms` (media
    /// minus media); `started_at` is what ages the session and orders the store. Handing
    /// the wall clock over as the epoch would put every event of a session that reused a
    /// scratch directory in the wrong place, which is the defect this parameter closes.
    ///
    /// The store cannot check that the epoch is plausible — it has no access to the
    /// recorder's ledger — and does not guess: it records what it is handed. `0` is the
    /// correct value for a session whose scratch directory is empty, which is the ordinary
    /// case.
    pub fn start_session(
        &self,
        game: Option<&str>,
        mode: &str,
        started_at_ms: i64,
        scratch_dir: &str,
        media_epoch_ms: i64,
    ) -> Result<i64> {
        if !SESSION_MODES.contains(&mode) {
            anyhow::bail!(
                "unknown session mode {mode:?}: this build knows {}",
                SESSION_MODES.join(", ")
            );
        }
        self.conn.execute(
            "INSERT INTO sessions (game, mode, started_at, scratch_dir, media_epoch_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![game, mode, started_at_ms, scratch_dir, media_epoch_ms],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Finish a session: stamp `ended_at`, record the final file and the bytes the session
    /// holds.
    ///
    /// # The two states this refuses, and why refusing is the boring answer
    ///
    /// `Result<()>` is the only channel this signature leaves for "what was there". A
    /// silent `Ok(())` for a session that does not exist, or for one that was already
    /// finalised, would be indistinguishable from success to the caller — which here means
    /// a recorder that believes it stamped the session file it just wrote, and a UI that
    /// believes the row it is showing is the row it just finished. So both come back as an
    /// `Err` that names the case, and the caller decides what to log:
    ///
    /// * no such session — the recorder is about to lose its bookkeeping, worth a warning;
    /// * already ended — the *first* finalisation wins and the row is not touched, so a
    ///   retry, a double stop, or a second writer cannot rewrite a finished session's
    ///   size or file. The `WHERE ... AND ended_at IS NULL` on the update makes that a
    ///   property of the write rather than of the read: the rowcount is checked, so a
    ///   session ended by another writer between the read and the write is reported, not
    ///   silently overwritten.
    ///
    /// This is the escape hatch for a session left running by a crash: a session whose
    /// recording was killed is still `ended_at IS NULL` (see
    /// [`Store::delete_session_returning_paths`]), and ending it here — with the clock the
    /// caller believes, a `final_path` of `None`, and the bytes it measured — is how a
    /// recovery pass makes it evictable again. That the caller must do it explicitly is
    /// the point: nothing infers "this session is over" from a missing process.
    pub fn end_session(
        &self,
        id: i64,
        ended_at_ms: i64,
        final_path: Option<&str>,
        size_bytes: i64,
    ) -> Result<()> {
        let ended: Option<Option<i64>> = self
            .conn
            .query_row("SELECT ended_at FROM sessions WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?;
        match ended {
            None => anyhow::bail!("cannot end session #{id}: no such session"),
            Some(Some(at)) => anyhow::bail!(
                "session #{id} was already ended at {at}ms; its row is untouched (the first \
                 finalisation of a session is the one that counts)"
            ),
            Some(None) => {}
        }

        let updated = self.conn.execute(
            "UPDATE sessions SET ended_at = ?2, final_path = ?3, size_bytes = ?4
             WHERE id = ?1 AND ended_at IS NULL",
            params![id, ended_at_ms, final_path, size_bytes],
        )?;
        if updated != 1 {
            anyhow::bail!(
                "session #{id} was ended by another writer between the read and the write; \
                 its row is untouched"
            );
        }
        Ok(())
    }

    /// Record how many bytes a running session currently holds.
    ///
    /// `started_at` is the only instant a session row has, so `size_bytes` is the only
    /// number the sessions retention cap can count — and [`Store::end_session`] is
    /// otherwise the only writer of it. Without this call a live recording is invisible to
    /// the cap: it would be planned against a session store that is missing the bytes the
    /// recording is currently writing, because `start_session` cannot know them. The
    /// recorder calls this on the tick it already scans the scratch ring for. `0` is the
    /// honest value for a session with no bytes on disk yet, and a session that is not
    /// running is refused — [`Store::end_session`] owns the finished number.
    pub fn set_session_size(&self, id: i64, size_bytes: i64) -> Result<()> {
        let ended: Option<Option<i64>> = self
            .conn
            .query_row("SELECT ended_at FROM sessions WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?;
        match ended {
            None => anyhow::bail!("cannot size session #{id}: no such session"),
            Some(Some(_)) => anyhow::bail!(
                "session #{id} has ended; its size is what end_session recorded, not a \
                 running total"
            ),
            Some(None) => {}
        }
        self.conn
            .execute("UPDATE sessions SET size_bytes = ?2 WHERE id = ?1", params![id, size_bytes])?;
        Ok(())
    }

    /// Every session, most recently started first — the order the review UI lists them in.
    ///
    /// Ordered by `started_at` and then by id, so the order is total and does not depend
    /// on the order SQLite happens to return rows in. The retention planner sorts the
    /// other way (oldest first) for itself; this is the library's own order.
    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions ORDER BY started_at DESC, id DESC"
        ))?;
        let rows = stmt.query_map([], read_session)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One session, or `None`.
    ///
    /// A missing id is `Ok(None)` and not an error: the caller asked a question, and "no
    /// such session" is the answer. An id that has been evicted, one a UI still has in a
    /// list, and one that never existed are the same state, and a caller that needs to
    /// report the difference has the id to name either way.
    pub fn get_session(&self, id: i64) -> Result<Option<Session>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"))?;
        Ok(stmt.query_row(params![id], read_session).optional()?)
    }

    /// The events recorded against one session, oldest first on the session timeline —
    /// the order the scrubber plots them in. **Nothing is filtered**: a manual hotkey
    /// bookmark and a game-derived kill or death are rows of the same timeline, and this
    /// method does not know the difference between the tags.
    ///
    /// `offset_ms` is derived here and only here:
    /// `events.at - sessions.media_epoch_ms`, one SQL expression, so the UI, the tests and
    /// any future consumer cannot disagree about what a position means. Both sides are media
    /// time — subtracting `started_at` instead was arithmetic across two clocks, which is
    /// what this used to do and why a session's events plotted in the wrong place.
    ///
    /// A session that does not exist has no events (an inner join), and so does one whose
    /// events were detached by its own deletion — in both cases the honest answer is an
    /// empty timeline rather than an error.
    pub fn events_for_session(&self, id: i64) -> Result<Vec<SessionEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.id, e.kind, e.at - s.media_epoch_ms, e.payload, e.clip_id
             FROM events e
             JOIN sessions s ON s.id = e.session_id
             WHERE e.session_id = ?1
             ORDER BY e.at, e.id",
        )?;
        let rows = stmt.query_map(params![id], |row| {
            Ok(SessionEvent {
                id: row.get(0)?,
                kind: row.get(1)?,
                offset_ms: row.get(2)?,
                payload: row.get(3)?,
                clip_id: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Mark a session as a favourite, or clear it. Favourites are exempt from both
    /// retention rules (spec §8.1), so this is the user's only way to protect a session
    /// from the manager.
    ///
    /// A session that does not exist is an `Err`, not a rest: `Result<()>` leaves nowhere
    /// else to say "this write touched nothing", and a silent success would let a UI
    /// report a favourite that is not stored. A caller that means "set it if it is still
    /// there" asks [`Store::get_session`] first.
    pub fn set_session_favourite(&self, id: i64, favourite: bool) -> Result<()> {
        let updated = self.conn.execute(
            "UPDATE sessions SET favourite = ?2 WHERE id = ?1",
            params![id, favourite as i64],
        )?;
        if updated == 0 {
            anyhow::bail!("no session #{id}: nothing was favourited");
        }
        Ok(())
    }

    /// Total bytes the indexed sessions claim, running ones included — the figure the
    /// sessions retention cap is measured against (see [`retention::RetentionRules`]).
    ///
    /// Like [`Store::total_bytes`], this is what the rows recorded, not a fresh walk of
    /// the scratch directories; the two only differ while a recording is running, and
    /// [`Store::set_session_size`] is how a recorder keeps them together.
    pub fn total_session_bytes(&self) -> Result<u64> {
        let bytes: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(size_bytes), 0) FROM sessions",
            [],
            |r| r.get(0),
        )?;
        Ok(bytes.max(0) as u64)
    }

    /// Delete the session row and hand back the paths so the caller can remove them
    /// **afterwards** — the same row-before-file contract as
    /// [`Store::delete_clip_returning_path`] (spec §8.2). Returns `None` if this call did
    /// not remove a row.
    ///
    /// **A session that is still recording is refused.** Every other deletion in this
    /// crate destroys a copy of something: a clip's footage, a scratch segment the ring
    /// can regenerate. The scratch directory of a *running* session is the recording —
    /// deleting it loses footage that exists nowhere else, and no ordering makes that
    /// safe. So `ended_at IS NULL` is a refusal with the reason in the message, and a
    /// session orphaned by a crash is made deletable by *ending* it (see
    /// [`Store::end_session`]) rather than by this method deciding the process is gone.
    ///
    /// **Both links to it are detached, and nothing else is.** `events.session_id` and
    /// `clips.session_id` are foreign keys into `sessions`; with enforcement on, deleting
    /// the session an event or a clip names would fail (`FOREIGN KEY constraint failed`)
    /// exactly when the manager tried to evict a session the user had clipped from. So
    /// both are set to NULL in the same transaction as the delete:
    ///
    /// * The **events survive as history** with `session_id` NULL — the same state every
    ///   event this engine has ever written is already in, and the same treatment
    ///   [`Store::delete_clip_returning_path`] gives an event whose clip went.
    /// * The **clips survive as first-class artefacts** with `session_id` NULL: a clip
    ///   extracted from a session is its own file with its own row, and evicting the
    ///   session it came from must not take the clip with it. The clip's footage lives in
    ///   the clips directory, which this method does not name and therefore cannot touch;
    ///   only the pointer to a row that no longer exists is cleared. `NULL` already means
    ///   "this clip belongs to no session" in this schema — every row written before
    ///   Phase 5 has it. (The declarative alternative, `ON DELETE SET NULL` on the two
    ///   foreign keys, needs a rebuild of `clips` and `events` to add; that is a migration
    ///   of its own and is deliberately not part of this one. See the `retention` module
    ///   for the same note.)
    pub fn delete_session_returning_paths(&self, id: i64) -> Result<Option<SessionPaths>> {
        let row: Option<(Option<i64>, Option<String>, String)> = self
            .conn
            .query_row(
                "SELECT ended_at, final_path, scratch_dir FROM sessions WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((ended_at, final_path, scratch_dir)) = row else {
            return Ok(None);
        };
        if ended_at.is_none() {
            anyhow::bail!(
                "session #{id} is still recording (ended_at IS NULL); refusing to delete it — \
                 its scratch directory is the recording. End it with end_session first."
            );
        }

        let tx = self.conn.unchecked_transaction()?;
        tx.execute("UPDATE events SET session_id = NULL WHERE session_id = ?1", params![id])?;
        tx.execute("UPDATE clips SET session_id = NULL WHERE session_id = ?1", params![id])?;
        let removed = tx.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok((removed == 1).then_some(SessionPaths { final_path, scratch_dir }))
    }
}

/// The `sessions` columns [`read_session`] expects, in order — one definition, so a
/// query and its reader cannot drift.
const SESSION_COLUMNS: &str = "id, game, mode, started_at, ended_at, scratch_dir, final_path, \
     size_bytes, favourite, media_epoch_ms";

fn read_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        game: row.get(1)?,
        mode: row.get(2)?,
        started_at_ms: row.get(3)?,
        ended_at_ms: row.get(4)?,
        scratch_dir: row.get(5)?,
        final_path: row.get(6)?,
        size_bytes: row.get(7)?,
        favourite: row.get::<_, i64>(8)? != 0,
        media_epoch_ms: row.get(9)?,
    })
}

/// Whether `table` has a column called `column`.
///
/// `PRAGMA table_info` cannot take a bound parameter, so the table name is interpolated —
/// it is a literal from this file, never caller input. Used by the v1 → v2 step to make
/// each `ADD COLUMN` idempotent on its own.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
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

    // ------------------------------------------------------------- sessions

    /// A session that is running: opened, not ended, and nothing on disk yet.
    fn running_session(store: &Store) -> i64 {
        store
            .start_session(Some("League of Legends"), SESSION_MODE_SESSION, 1_000_000, "/scratch/1", 0)
            .expect("open the session")
    }

    /// `clips.session_id` for one row.
    ///
    /// `Clip` does not carry the column — the clip list is the library, not the session
    /// tree — and the link is written by the session recorder, which owns that decision.
    /// A test therefore reads the column the way an operator would.
    fn clip_session_id(store: &Store, clip_id: i64) -> Option<i64> {
        store
            .conn
            .query_row("SELECT session_id FROM clips WHERE id = ?1", params![clip_id], |r| r.get(0))
            .expect("the clip row")
    }

    #[test]
    fn a_session_is_opened_read_back_and_ended() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let id = running_session(&s);

        let running = s.get_session(id).expect("the session").expect("it is there");
        assert_eq!(running.id, id);
        assert_eq!(running.game.as_deref(), Some("League of Legends"));
        assert_eq!(running.mode, SESSION_MODE_SESSION);
        assert_eq!(running.started_at_ms, 1_000_000);
        assert_eq!(running.ended_at_ms, None, "a session is running until it is ended");
        assert_eq!(running.scratch_dir, "/scratch/1");
        assert_eq!(running.final_path, None, "there is no session file yet");
        assert_eq!(running.size_bytes, 0);
        assert!(!running.favourite, "a new session is not a favourite");

        s.end_session(id, 1_120_000, Some("/sessions/1.mp4"), 4_096).expect("the session ends");
        let ended = s.get_session(id).expect("the session").expect("it is still there");
        assert_eq!(ended.ended_at_ms, Some(1_120_000));
        assert_eq!(ended.final_path.as_deref(), Some("/sessions/1.mp4"));
        assert_eq!(ended.size_bytes, 4_096);
    }

    #[test]
    fn a_session_may_have_no_game_and_an_unknown_mode_is_refused() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let id = s
            .start_session(None, SESSION_MODE_BUFFER, 5, "/scratch/2", 0)
            .expect("the session opens");
        assert_eq!(
            s.get_session(id).expect("the session").expect("the session row").game,
            None,
            "a session with no game"
        );

        // A typo must not be stored: every reader of `mode` switches on the column.
        let err = s
            .start_session(None, "ful-session", 6, "/scratch/3", 0)
            .expect_err("an unknown mode is refused");
        assert!(format!("{err:#}").contains("unknown session mode"), "{err:#}");
        assert_eq!(s.list_sessions().expect("the sessions").len(), 1, "and nothing was written");
    }

    #[test]
    fn ending_a_session_twice_is_reported_and_the_first_finalisation_stands() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let id = running_session(&s);
        s.end_session(id, 1_100_000, Some("/sessions/1.mp4"), 4_096).expect("the session ends");

        let err = s
            .end_session(id, 1_200_000, Some("/sessions/other.mp4"), 9_999)
            .expect_err("a second finalisation is refused, not silently ignored");
        assert!(format!("{err:#}").contains("already ended"), "{err:#}");

        let row = s.get_session(id).expect("the session").expect("the session row");
        assert_eq!(row.ended_at_ms, Some(1_100_000), "the first end time still stands");
        assert_eq!(row.final_path.as_deref(), Some("/sessions/1.mp4"));
        assert_eq!(row.size_bytes, 4_096);
    }

    #[test]
    fn ending_a_session_that_does_not_exist_is_reported_rather_than_assumed() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let err = s
            .end_session(42, 1, None, 0)
            .expect_err("there is no session 42 — a silent Ok would read as success");
        assert!(format!("{err:#}").contains("no such session"), "{err:#}");
        assert!(s.list_sessions().expect("the sessions").is_empty());
    }

    #[test]
    fn getting_a_session_that_does_not_exist_is_none_with_no_events() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        assert_eq!(s.get_session(7).expect("the session"), None, "no such session, no error, no panic");
        assert!(s.events_for_session(7).expect("the timeline").is_empty(), "and no timeline to plot");
    }

    #[test]
    fn sessions_are_listed_newest_started_first() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let first = s
            .start_session(None, SESSION_MODE_BUFFER, 1_000, "/scratch/a", 0)
            .expect("the session opens");
        let second = s
            .start_session(None, SESSION_MODE_BUFFER, 2_000, "/scratch/b", 0)
            .expect("the session opens");
        // A tie on `started_at`: the id breaks it, so the order is total.
        let tied = s
            .start_session(None, SESSION_MODE_BUFFER, 2_000, "/scratch/c", 0)
            .expect("the session opens");

        assert_eq!(
            s.list_sessions().expect("the sessions").iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![tied, second, first],
            "most recently started first"
        );
    }

    #[test]
    fn a_session_timeline_is_chronological_with_offsets_from_the_start() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        // Two clocks, deliberately different numbers: the timeline must be measured from the
        // MEDIA epoch, not the wall start. The old code subtracted `started_at` (wall) from
        // `events.at` (media) — arithmetic across two clocks, which put every event of a
        // session that reused a scratch directory in the wrong place. With the two
        // accidentally equal, as this fixture used to have them, that looked correct.
        let session = s
            .start_session(
                Some("League of Legends"),
                SESSION_MODE_SESSION,
                1_700_000_000_000, // wall: when the recording began
                "/scratch/timeline",
                1_000_000, // media: where the ledger stood when it began
            )
            .expect("open the session");
        let clip = s.insert_clip(&NewClip {
            path: "/clips/kill.mp4".into(),
            started_at_ms: 30_000,
            duration_ms: 12_000,
            size_bytes: 100,
            codec: "h264_nvenc".into(),
        }).expect("the clip is indexed");

        // Deliberately inserted out of order, and deliberately of every provenance: a
        // manual hotkey bookmark, a game-derived kill that produced a clip, a
        // game-derived death, a marker with no clip, and one event before the session's
        // own start instant.
        for (kind, at, clip_id) in [
            ("kill", 1_030_000u64, Some(clip)),
            ("bookmark", 1_000_000, None),
            ("death", 1_045_000, None),
            ("round_start", 1_020_000, None),
            ("assist", 999_999, None),
        ] {
            s.insert_event(&NewEvent {
                session_id: Some(session),
                kind: kind.into(),
                at_ms: at,
                payload: Some(format!("{{\"source\":\"test\",\"event\":\"{kind}\"}}")),
                clip_id,
            }).expect("the event is written");
        }
        // An event of another session must not appear; nor must one with no session.
        let other = s
            .start_session(None, SESSION_MODE_BUFFER, 2_000_000, "/scratch/other", 0)
            .expect("the session opens");
        s.insert_event(&an_event("kill", 2_000_001, None)).expect("the event is written");
        s.insert_event(&an_event("kill", 2_000_002, None)).expect("the event is written");
        s.conn
            .execute("UPDATE events SET session_id = ?2 WHERE at = ?1", params![2_000_001, other])
            .expect("the statement");

        let timeline = s.events_for_session(session).expect("the timeline");
        assert_eq!(
            timeline.iter().map(|e| (e.kind.as_str(), e.offset_ms)).collect::<Vec<_>>(),
            vec![
                ("assist", -1),
                ("bookmark", 0),
                ("round_start", 20_000),
                ("kill", 30_000),
                ("death", 45_000),
            ],
            "oldest first, offsets from the session start, and nothing filtered by kind"
        );
        assert_eq!(
            timeline[1].offset_ms, 0,
            "an event at the session's own start instant is exactly 0"
        );
        assert_eq!(timeline[3].clip_id, Some(clip), "the clip an event produced travels too");
        assert!(timeline[1].clip_id.is_none(), "a bookmark that produced no clip says so");
        assert!(
            timeline[3]
                .payload
                .as_deref()
                .expect("the payload is the JSON the caller stored")
                .contains("kill")
        );

        let other_timeline = s.events_for_session(other).expect("the timeline");
        assert_eq!(other_timeline.len(), 1, "one event on the other session");
    }

    #[test]
    fn favouriting_a_session_reports_a_missing_id() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let id = running_session(&s);

        s.set_session_favourite(id, true).expect("the toggle");
        assert!(s.get_session(id).expect("the session").expect("the session row").favourite);
        s.set_session_favourite(id, false).expect("the toggle");
        assert!(!s.get_session(id).expect("the session").expect("the session row").favourite);

        let err = s.set_session_favourite(id + 1, true).expect_err("no such session");
        assert!(format!("{err:#}").contains("no session"), "{err:#}");
    }

    #[test]
    fn a_running_sessions_size_is_what_the_cap_can_see() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let id = running_session(&s);
        assert_eq!(
            s.total_session_bytes().expect("the session store's bytes"),
            0,
            "an empty session store holds nothing"
        );

        // A recorder keeps the running total current on the tick it scans its ring; without
        // this the sessions cap would plan against zero bytes for a live recording.
        s.set_session_size(id, 3_000).expect("the running size");
        assert_eq!(s.total_session_bytes().expect("the session store's bytes"), 3_000);

        // Once it has ended, `end_session` owns the number.
        s.end_session(id, 1_100_000, Some("/sessions/1.mp4"), 4_096).expect("the session ends");
        assert_eq!(s.total_session_bytes().expect("the session store's bytes"), 4_096);
        assert!(s.set_session_size(id, 1).is_err(), "a finished session's size is final");
        assert!(s.set_session_size(id + 1, 1).is_err(), "and there is no session to size");
    }

    #[test]
    fn deleting_a_session_detaches_its_events_and_keeps_its_clips() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let session = running_session(&s);
        s.end_session(session, 1_120_000, Some("/sessions/1.mp4"), 9_000).expect("the session ends");

        // A clip extracted from the session. `insert_clip` takes no session_id — its
        // signature is frozen for this pass — so the link is written the way the session
        // recorder writes it, and the foreign key would refuse the delete below if the
        // store did not detach it.
        let clip = s.insert_clip(&NewClip {
            path: "/clips/from-session.mp4".into(),
            started_at_ms: 30_000,
            duration_ms: 12_000,
            size_bytes: 100,
            codec: "h264_nvenc".into(),
        }).expect("the clip is indexed");
        s.conn
            .execute("UPDATE clips SET session_id = ?2 WHERE id = ?1", params![clip, session])
            .expect("the statement");
        assert_eq!(clip_session_id(&s, clip), Some(session), "the clip is linked to the session");
        let event = s.insert_event(&NewEvent {
            session_id: Some(session),
            kind: "kill".into(),
            at_ms: 1_030_000,
            payload: None,
            clip_id: Some(clip),
        }).expect("the event is written");

        let paths = s.delete_session_returning_paths(session).expect("the session is deleted");
        assert_eq!(
            paths,
            Some(SessionPaths {
                final_path: Some("/sessions/1.mp4".into()),
                scratch_dir: "/scratch/1".into(),
            }),
            "the row names what the caller must remove, and nothing else"
        );

        assert_eq!(s.get_session(session).expect("the session"), None, "the session row is gone");
        let clips = s.list_clips().expect("the clips");
        assert_eq!(clips.len(), 1, "an extracted clip is a first-class artefact and survives");
        assert_eq!(clips[0].id, clip);
        assert_eq!(clips[0].path, PathBuf::from("/clips/from-session.mp4"));
        assert_eq!(clip_session_id(&s, clip), None, "only the link to the gone row is cleared");
        let events = s.list_events().expect("the events");
        assert_eq!(events.len(), 1, "the event survives as history");
        assert_eq!(events[0].id, event);
        assert_eq!(events[0].session_id, None, "and no longer names a row that is gone");
        assert_eq!(events[0].clip_id, Some(clip), "the clip it produced is still there");
        assert!(
            s.events_for_session(session).expect("the timeline").is_empty(),
            "the timeline of a session that no longer exists is empty, not an error"
        );
        assert_eq!(
            s.delete_session_returning_paths(session).expect("the session is deleted"),
            None,
            "a second delete is a no-op"
        );
    }

    #[test]
    fn a_session_that_is_still_recording_cannot_be_deleted() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let id = running_session(&s);

        // The scratch directory of a running session *is* the recording: there is no second
        // copy, so no ordering makes deleting it safe.
        let err = s
            .delete_session_returning_paths(id)
            .expect_err("a running session must not be deletable");
        assert!(format!("{err:#}").contains("still recording"), "{err:#}");
        assert!(s.get_session(id).expect("the session").is_some(), "the row is untouched");

        // The escape hatch is explicit: end it — with the clock the caller believes and the
        // bytes it measured — and it becomes an ordinary deletable session. A session left
        // running by a crash is the same case, and this is how a recovery pass resolves it.
        s.end_session(id, 1_200_000, None, 0).expect("the session ends");
        let paths = s.delete_session_returning_paths(id).expect("the session is deleted");
        assert_eq!(
            paths,
            Some(SessionPaths { final_path: None, scratch_dir: "/scratch/1".into() }),
            "a session with no final file hands back only its scratch directory"
        );
        assert_eq!(
            s.delete_session_returning_paths(id + 1).expect("the session is deleted"),
            None,
            "no such row"
        );
    }

    /// A database with the v1 tables, v1's `user_version`, and a row in each table.
    fn v1_database(path: &Path) -> Store {
        let conn = Connection::open(path).expect("open the v1 database");
        conn.execute_batch(V1_DDL).expect("the v1 tables, exactly as v1 shipped them");
        conn.execute_batch("PRAGMA user_version = 1").expect("stamp v1");
        conn.execute(
            "INSERT INTO sessions (id, game, started_at, ended_at, scratch_dir)
             VALUES (1, 'League of Legends', 1000, 61000, '/app/scratch')",
            [],
        )
        .expect("a v1 session row");
        conn.execute(
            "INSERT INTO clips (id, session_id, path, started_at, duration_ms, size_bytes, codec, created_at)
             VALUES (1, 1, '/app/clips/kill.mp4', 30000, 12000, 500000, 'h264_nvenc', 7000)",
            [],
        )
        .expect("a v1 clip row, linked to the session");
        conn.execute(
            "INSERT INTO events (id, session_id, kind, at, payload, clip_id)
             VALUES (1, 1, 'kill', 30000, '{\"source\":\"gsi\"}', 1)",
            [],
        )
        .expect("a v1 event row");
        drop(conn);
        Store::open(path).expect("open the v1 database through the store")
    }

    /// The column names, declared types and nullability of `table`, as SQLite sees them.
    fn columns(store: &Store, table: &str) -> Vec<(String, String, i64)> {
        let mut stmt = store.conn.prepare(&format!("PRAGMA table_info({table})")).expect("table_info");
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)))
            .expect("query");
        rows.collect::<rusqlite::Result<Vec<_>>>().expect("columns")
    }

    #[test]
    fn a_v1_database_is_upgraded_in_place_and_loses_no_rows() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("localplay.db");
        let store = v1_database(&path);
        assert_eq!(store.schema_version().expect("version"), 1, "the fixture is a v1 database");

        store.migrate().expect("the upgrade");

        assert_eq!(
            store.schema_version().expect("version"),
            SCHEMA_VERSION,
            "the upgrade lands on the version this build writes"
        );

        // The v1 rows are all still there, with the values they had.
        let sessions = store.list_sessions().expect("the sessions");
        assert_eq!(sessions.len(), 1, "the legacy session survived");
        assert_eq!(sessions[0].game.as_deref(), Some("League of Legends"));
        assert_eq!(
            sessions[0].media_epoch_ms, 0,
            "a legacy row's media epoch is 0 — the only value its own contents support, and \
             the correct one for the ordinary case of a fresh scratch directory"
        );
        assert_eq!(sessions[0].started_at_ms, 1_000);
        assert_eq!(sessions[0].ended_at_ms, Some(61_000));
        assert_eq!(sessions[0].scratch_dir, "/app/scratch");
        let clips = store.list_clips().expect("the clips");
        assert_eq!(clips.len(), 1, "the legacy clip survived");
        assert_eq!(clips[0].path, PathBuf::from("/app/clips/kill.mp4"));
        assert_eq!(clips[0].size_bytes, 500_000);
        assert_eq!(clips[0].created_at_ms, 7_000);
        let events = store.list_events().expect("the events");
        assert_eq!(events.len(), 1, "the legacy event survived");
        assert_eq!(events[0].session_id, Some(1), "and still names its session");
        assert_eq!(events[0].clip_id, Some(1));

        // The new columns exist, with the defaults a legacy row must get.
        assert_eq!(
            sessions[0].mode, SESSION_MODE_BUFFER,
            "a legacy row was written by the buffer engine or not at all: 'buffer' is the \
             only mode that explains its own contents"
        );
        assert_eq!(sessions[0].final_path, None, "a legacy session has no session file");
        assert_eq!(sessions[0].size_bytes, 0, "and no recorded size to invent one from");
        assert!(!sessions[0].favourite, "favourites default to not favourited");
        for column in ["mode", "final_path", "size_bytes", "favourite"] {
            assert!(column_exists(&store.conn, "sessions", column).expect("table_info"), "{column}");
        }

        // The upgraded shape is the shape a new database gets — no drift between a database
        // this build created and one it upgraded.
        let fresh = Store::open_in_memory().expect("a fresh store");
        fresh.migrate().expect("migrate");
        assert_eq!(columns(&store, "sessions"), columns(&fresh, "sessions"));
        assert_eq!(columns(&store, "clips"), columns(&fresh, "clips"));
        assert_eq!(columns(&store, "events"), columns(&fresh, "events"));

        // The timeline of the legacy session is readable through the new API. Its epoch is
        // the migration's default, so the offset is the event's own media position rather
        // than a difference: this row was written before v3, which means it never recorded an
        // epoch, and 0 is the only value its contents support. For the ordinary case — a
        // session against a fresh scratch directory — that IS the session-relative offset.
        //
        // (The old code subtracted `started_at`, the wall clock, from a media timestamp:
        // arithmetic across two clocks, which is the defect the column exists to fix.)
        let timeline = store.events_for_session(1).expect("the legacy timeline");
        assert_eq!(timeline.len(), 1);
        assert_eq!(
            timeline[0].offset_ms, 30_000,
            "a legacy row's epoch defaults to 0, so the offset is the event's own position"
        );
        store.migrate().expect("migrating again");
        assert_eq!(
            store.schema_version().expect("version"),
            SCHEMA_VERSION,
            "and it stays at the current version"
        );
        assert_eq!(store.list_sessions().expect("sessions").len(), 1);
    }

    #[test]
    fn the_v1_to_v2_step_is_safe_on_a_database_that_already_has_a_v2_column() {
        // An upgrade interrupted by a build that added the columns one at a time, or a
        // hand-edited file: `ADD COLUMN` is not idempotent on its own, so the step checks
        // each column before adding it.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("half-migrated.db");
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch(V1_DDL).expect("v1 tables");
            conn.execute_batch("ALTER TABLE sessions ADD COLUMN mode TEXT NOT NULL DEFAULT 'buffer'")
                .expect("one of v2's columns is already there");
            conn.execute_batch("PRAGMA user_version = 1").expect("still stamped v1");
            conn.execute(
                "INSERT INTO sessions (id, game, started_at, ended_at, scratch_dir, mode)
                 VALUES (1, NULL, 5, 6, '/scratch/x', 'session')",
                [],
            )
            .expect("a row that already has a mode");
        }

        let store = Store::open(&path).expect("open through the store");
        store.migrate().expect("the upgrade must not die on a duplicate column");
        assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
        for column in ["mode", "final_path", "size_bytes", "favourite", "media_epoch_ms"] {
            assert!(column_exists(&store.conn, "sessions", column).expect("table_info"), "{column}");
        }
        let session = store.get_session(1).expect("read").expect("the row survived");
        assert_eq!(session.mode, SESSION_MODE_SESSION, "an existing value is never overwritten");
    }

    #[test]
    fn a_database_newer_than_this_build_is_refused_and_left_alone() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("from-the-future.db");
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch(V1_DDL).expect("tables");
            conn.execute_batch("PRAGMA user_version = 99").expect("a newer build's version");
            conn.execute(
                "INSERT INTO sessions (id, game, started_at, ended_at, scratch_dir)
                 VALUES (1, NULL, 1, 2, '/scratch/future')",
                [],
            )
            .expect("a row");
        }

        let store = Store::open(&path).expect("open through the store");
        let err = store.migrate().expect_err("a newer schema must be refused, never downgraded");
        assert!(format!("{err:#}").contains("newer than this build supports"), "{err:#}");
        assert_eq!(store.schema_version().expect("version"), 99, "the version is not rewritten");
        // Read through SQL rather than the session API: a refused v99 database has no `mode`
        // column to read, which is the fail-closed outcome and not a second failure. What
        // matters is that the row is still there.
        let rows: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .expect("the v1 row is readable");
        assert_eq!(rows, 1, "and nothing was truncated");
        assert!(
            !column_exists(&store.conn, "sessions", "final_path").expect("table_info"),
            "a refused migration changes no structure either"
        );
    }
}
