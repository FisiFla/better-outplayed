//! The cleanup executor against a real database and real files (spec §8.2).
//!
//! These tests are about one property, stated in the spec as a safety requirement rather
//! than a detail: **the database row is deleted and committed before the file is
//! unlinked, and no file is deleted unless a committed database row names it.**
//!
//! The plan itself is pure and covered exhaustively in `crates/store/src/cleanup.rs`;
//! here a real [`Store`] over a real SQLite file, a real clips directory, and real
//! unlinks show what the executor does to both — including the two ways the filesystem
//! can disagree with the index (a missing file, an unlink that fails), and the case that
//! must delete nothing at all (a plan id whose row is already gone).

use localplay_store::cleanup::{execute_cleanup, plan_cleanup, CleanupPolicy};
use localplay_store::{NewClip, Store};
use std::path::PathBuf;

/// A clips directory plus the database that indexes it, in a temp dir that outlives the
/// test body.
struct Fixture {
    _dir: tempfile::TempDir,
    clips_dir: PathBuf,
    db_path: PathBuf,
    store: Store,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clips_dir = dir.path().join("clips");
        std::fs::create_dir_all(&clips_dir).expect("create the clips dir");
        let db_path = dir.path().join("localplay.db");
        let store = Store::open(&db_path).expect("open the clip index");
        store.migrate().expect("migrate the clip index");
        Self { _dir: dir, clips_dir, db_path, store }
    }

    /// Write a clip file with `bytes` of content, index it, and return its row id and
    /// path — the same pair a real clip leaves behind.
    fn add_clip(&self, name: &str, bytes: usize) -> (i64, PathBuf) {
        let path = self.clips_dir.join(name);
        std::fs::write(&path, vec![0u8; bytes]).expect("write the clip file");
        let id = self
            .store
            .insert_clip(&NewClip {
                path: path.clone(),
                started_at_ms: 1_000,
                duration_ms: 1_000,
                size_bytes: bytes as u64,
                codec: "h264_nvenc".into(),
            })
            .expect("index the clip");
        (id, path)
    }

    /// Index a path without writing anything at it.
    fn index_only(&self, path: PathBuf, bytes: u64) -> i64 {
        self.store
            .insert_clip(&NewClip {
                path,
                started_at_ms: 1_000,
                duration_ms: 1_000,
                size_bytes: bytes,
                codec: "h264_nvenc".into(),
            })
            .expect("index the clip")
    }

    fn ids(&self) -> Vec<i64> {
        self.store.list_clips().expect("list").iter().map(|c| c.id).collect()
    }

    /// Backdate a row's creation time, the way an operator would with `sqlite3`.
    ///
    /// The application deliberately has no API for this — `created_at` is stamped by the
    /// store when a clip is indexed, never by the caller — so a test that wants an aged
    /// clip has to reach the column the way a real aged library would have got there: the
    /// row was written days ago. The clock is the only thing faked here.
    fn backdate(&self, id: i64, created_at_ms: i64) {
        let conn = rusqlite::Connection::open(&self.db_path).expect("a second connection");
        conn.execute(
            "UPDATE clips SET created_at = ?2 WHERE id = ?1",
            rusqlite::params![id, created_at_ms],
        )
        .expect("backdate the row");
    }

    /// Plan against the store's own rows, as the application does.
    fn plan(&self, policy: &CleanupPolicy) -> localplay_store::cleanup::CleanupPlan {
        plan_cleanup(&self.store.list_clips().expect("list"), policy, now_ms())
    }
}

fn policy(max_total_bytes: u64, max_age_days: u64) -> CleanupPolicy {
    CleanupPolicy { max_total_bytes, max_age_days }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A file nobody indexes, in the clips directory the executor is working in.
fn stray_file(clips_dir: &std::path::Path) -> PathBuf {
    let stray = clips_dir.join("unindexed.bin");
    std::fs::write(&stray, b"not a clip, not indexed").expect("write the stray file");
    stray
}

#[test]
fn rows_and_files_are_deleted_and_a_favourite_is_left_alone() {
    let fx = Fixture::new();
    let (a, a_path) = fx.add_clip("a.mp4", 100); // created first: evicted first
    let (b, b_path) = fx.add_clip("b.mp4", 100);
    let (fav, fav_path) = fx.add_clip("fav.mp4", 300);
    assert!(fx.store.set_favourite(fav, true).expect("favourite the clip"));
    let stray = stray_file(&fx.clips_dir);
    for path in [&a_path, &b_path, &fav_path, &stray] {
        assert!(path.is_file(), "fixture: {} must exist", path.display());
    }

    // 500 bytes over a 300 byte cap; the favourite alone accounts for the whole cap.
    let plan = fx.plan(&policy(300, 3_650));
    assert_eq!(plan.ids(), vec![a, b], "the two oldest non-favourites");
    assert!(plan.cap_met(), "the favourites are inside the cap, so it is reachable");
    assert_eq!(plan.bytes_after, 300);

    let outcome = execute_cleanup(&fx.store, &plan).expect("execute the plan");

    assert_eq!(outcome.deleted, 2);
    assert_eq!(outcome.failed, 0);
    assert_eq!(outcome.bytes_reclaimed, 200);
    assert!(outcome.orphaned.is_empty(), "both files were where the rows said");
    assert!(outcome.already_missing.is_empty());
    assert_eq!(outcome.bytes_after, 300, "re-read from the store after the deletes");
    assert!(outcome.cap_met);

    assert_eq!(fx.ids(), vec![fav], "the rows are gone");
    assert!(!a_path.exists(), "{} must be unlinked", a_path.display());
    assert!(!b_path.exists(), "{} must be unlinked", b_path.display());
    assert!(fav_path.exists(), "a favourite is never deleted, file included");
    assert!(stray.exists(), "the executor only ever unlinks paths a deleted row named");
}

#[test]
fn an_aged_clip_is_deleted_and_a_recent_one_is_kept() {
    let fx = Fixture::new();
    let (old, old_path) = fx.add_clip("old.mp4", 100);
    let (recent, recent_path) = fx.add_clip("recent.mp4", 100);
    fx.backdate(old, now_ms() - 30 * 86_400_000);

    // The age rule alone: the cap is not in play at all.
    let plan = fx.plan(&policy(u64::MAX, 7));
    assert_eq!(plan.ids(), vec![old], "only the aged clip");
    assert!(plan.deletions[0].reasons.too_old);

    let outcome = execute_cleanup(&fx.store, &plan).expect("execute the plan");

    assert_eq!(outcome.deleted, 1);
    assert_eq!(outcome.bytes_reclaimed, 100);
    assert!(outcome.cap_met);
    assert_eq!(fx.ids(), vec![recent], "the recent clip's row is untouched");
    assert!(!old_path.exists(), "the aged clip's file is gone with its row");
    assert!(recent_path.exists(), "the recent clip's file is untouched");
}

#[test]
fn a_row_whose_file_is_already_gone_is_deleted_and_reported() {
    let fx = Fixture::new();
    let missing = fx.clips_dir.join("missing.mp4");
    let id = fx.index_only(missing.clone(), 100);
    let stray = stray_file(&fx.clips_dir);
    assert!(!missing.exists());

    let plan = fx.plan(&policy(0, 3_650));
    assert_eq!(plan.ids(), vec![id]);

    let outcome = execute_cleanup(&fx.store, &plan).expect("execute the plan");

    assert_eq!(outcome.deleted, 1, "the row is deleted even though the file is not there");
    assert_eq!(
        outcome.already_missing,
        vec![missing.clone()],
        "a row pointing at nothing is reported, not swallowed"
    );
    assert!(outcome.orphaned.is_empty(), "there is no file on disk to orphan");
    assert_eq!(outcome.bytes_reclaimed, 0, "nothing was reclaimed: nothing was removed");
    assert!(fx.ids().is_empty(), "no rows are left");
    assert!(stray.exists(), "the unlink never moved away from the row's own path");
}

#[test]
fn an_unlink_that_fails_is_reported_as_an_orphan_and_nothing_else_is_touched() {
    let fx = Fixture::new();
    let (fav, fav_path) = fx.add_clip("fav.mp4", 100);
    assert!(fx.store.set_favourite(fav, true).expect("favourite the clip"));
    let stray = stray_file(&fx.clips_dir);

    // A path a row names that `remove_file` cannot remove: a directory, as a directory
    // named `*.mp4` would be if a user (or a half-finished copy) put one there.
    let doomed = fx.clips_dir.join("doomed.mp4");
    std::fs::create_dir_all(&doomed).expect("create the directory");
    std::fs::write(doomed.join("inside.bin"), b"still here").expect("write into it");
    let id = fx.index_only(doomed.clone(), 100);

    // 200 bytes over a 100 byte cap, and the favourite alone is exactly the cap, so the
    // size rule is satisfiable and the doomed clip is what it evicts.
    let plan = fx.plan(&policy(100, 3_650));
    assert_eq!(plan.ids(), vec![id]);

    let outcome = execute_cleanup(&fx.store, &plan).expect("execute the plan");

    assert_eq!(outcome.deleted, 1, "the row is deleted first, and it stays deleted");
    assert_eq!(
        outcome.orphaned,
        vec![doomed.clone()],
        "the unlink that failed is reported with its path"
    );
    assert!(outcome.already_missing.is_empty());
    assert_eq!(outcome.bytes_reclaimed, 0);
    assert!(doomed.is_dir(), "the file the unlink could not remove is still there");
    assert!(
        doomed.join("inside.bin").exists(),
        "and the executor did not reach inside it"
    );
    assert!(fav_path.exists(), "the favourite is untouched — it was not in the plan");
    assert!(stray.exists(), "and neither is anything the index does not name");
    assert_eq!(fx.ids(), vec![fav], "only the doomed clip's row is gone");
}

#[test]
fn a_plan_id_whose_row_is_already_gone_deletes_nothing() {
    let fx = Fixture::new();
    let (id, path) = fx.add_clip("keep.mp4", 100);
    let plan = fx.plan(&policy(0, 3_650));
    assert_eq!(plan.ids(), vec![id]);

    // The row disappears between planning and executing — another cleanup pass, a second
    // instance, a hand-edited index. This is the case the ordering exists for: with no
    // committed row delete, there is no authority to unlink anything.
    let removed = fx.store.delete_clip_returning_path(id).expect("delete the row");
    assert_eq!(removed.map(PathBuf::from), Some(path.clone()));

    let outcome = execute_cleanup(&fx.store, &plan).expect("execute the plan");

    assert_eq!(outcome.deleted, 0);
    assert_eq!(outcome.bytes_reclaimed, 0);
    assert!(path.exists(), "the file is still there: no row named it any more");
    assert!(outcome.orphaned.is_empty());
}
