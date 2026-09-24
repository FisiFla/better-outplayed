//! Retention for the two classes of artefact this application keeps: **clips** and
//! **sessions** (spec §8.1, extended to the session store).
//!
//! [`crate::cleanup`] is one flat policy over one library: the clips directory, driven by
//! a single [`crate::cleanup::CleanupPolicy`] and [`crate::cleanup::plan_cleanup`]. That
//! shape cannot express what Phase 5 needs, because a full-session recording is a
//! *different class of artefact* — bigger, longer-lived, and managed in units of whole
//! sessions rather than whole clips. A single `max_total_bytes` over both would mean a
//! user recording sessions at 100 GB loses every clip they had, or the reverse: whichever
//! class grew first would evict the other. So this module adds a second policy *beside* the
//! first, and
//!
//! * a session store over its cap never evicts a clip,
//! * a clips store over its cap never evicts a session,
//! * both use the same two rules, the same reason flags and the same deletion ordering.
//!
//! Nothing in [`crate::cleanup`] changes: the clips class of [`plan_retention`] **is**
//! [`plan_cleanup`], called with the clips half of the policy and converted into this
//! module's plan shape, so the two entry points cannot drift into disagreeing about the
//! same library.
//!
//! # Deliberate duplication
//!
//! Two shapes here parallel their [`crate::cleanup`] counterparts:
//!
//! * [`RetentionDeletion`] is [`crate::cleanup::PlannedDeletion`] with [`Subject`] in place
//!   of a bare `id` — a plan that names sessions and clips has to say which class an id
//!   belongs to, or two artefacts could share one.
//! * [`ClassOutcome`] is [`crate::cleanup::CleanupOutcome`] plus the one field a class that
//!   can refuse to remove a path needs.
//!
//! Both could be fused with their originals by widening the existing types, and that is
//! exactly what is deferred: this pass is additive, `CleanupOutcome` and `PlannedDeletion`
//! are part of signatures other crates already build against, and the fusion is a
//! mechanical refactor for a later pass rather than a change to make while three agents
//! are editing the workspace. [`crate::cleanup::DeletionReasons`] — the "why" itself — is
//! **reused verbatim**, so a plan from here and a plan from there explain themselves
//! identically.

use crate::cleanup::{plan_cleanup, CleanupPolicy, DeletionReasons};
use crate::{Clip, Session, Store};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Milliseconds in a day; the age rules' unit conversion.
///
/// A private copy of the constant in [`crate::cleanup`], which is private there and would
/// have to be widened to be shared — a one-line edit to a file this pass must not touch.
/// The two values are the same length of day or neither is right, so this is noted as a
/// duplication rather than hidden.
const MS_PER_DAY: i64 = 24 * 60 * 60 * 1_000;

/// Retention for one class of artefact: the two rules of spec §8.1, per class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionRules {
    /// Total bytes this class may occupy, its favourites included. An unsatisfiable cap
    /// is *reported*, never paid for out of the class's other artefacts (see
    /// [`ClassPlan::over_cap_by_bytes`]).
    pub max_total_bytes: u64,
    /// A member of the class older than this many days is deleted, unless it is a
    /// favourite (or, for a session, still recording). As in
    /// [`CleanupPolicy::max_age_days`], `0` means "everything older than `now_ms`", and
    /// the comparison is strictly greater — "older than", not "at least as old as".
    pub max_age_days: u32,
}

/// Sessions and clips are evicted independently: a session store at 100 GB must not push
/// clips out, and vice versa.
///
/// Two independent rules over two independent libraries — not one cap shared, which could
/// only ever be satisfied by evicting the class that grew first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub clips: RetentionRules,
    pub sessions: RetentionRules,
}

/// What a planned deletion refers to. Sessions are deleted as units: there is no such
/// thing as evicting half a session, and a session's segments are not independently
/// addressable rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    Clip(i64),
    Session(i64),
}

impl Subject {
    /// The row id, whichever class it is — for logging and for assertions.
    pub fn id(self) -> i64 {
        match self {
            Subject::Clip(id) | Subject::Session(id) => id,
        }
    }

    /// `"clip"` or `"session"`, for a log line that says which class it is about.
    pub fn class(self) -> &'static str {
        match self {
            Subject::Clip(_) => "clip",
            Subject::Session(_) => "session",
        }
    }
}

/// One artefact a plan deletes, with the bytes its row accounts for and **why** it is
/// going.
///
/// The reasons are [`DeletionReasons`], shared with the clips policy: an artefact both
/// rules select appears once, flagged with both, and the flag is what the log line and the
/// UI show. A deletion is never anonymous ("something had to go") — `too_old` is a
/// retention decision and `over_cap` is a size decision, and they are different things to
/// tell a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionDeletion {
    pub subject: Subject,
    pub size_bytes: u64,
    pub reasons: DeletionReasons,
}

/// What a plan does to one class, and where that leaves it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClassPlan {
    /// Members to delete, oldest first (see [`plan_retention`]), each exactly once.
    pub deletions: Vec<RetentionDeletion>,
    /// The cap this class was planned against, carried so the executor can check the
    /// post-condition against the live store rather than against this forecast.
    pub cap_bytes: u64,
    /// Total size of the class if every deletion here succeeds.
    pub bytes_after: u64,
    /// The part of the overshoot that **no** pass may ever remove: how far the members that
    /// are immune to eviction — favourites, and sessions that are still recording — exceed
    /// the cap.
    ///
    /// Non-zero means the cap is unsatisfiable under the policy, and the size rule selects
    /// nothing at all for this class, for the reason spec §8.1 gives: evicting the
    /// evictable could not reach the cap anyway, and doing it would destroy the material
    /// the user did not protect. The caller is expected to warn with this number instead.
    pub over_cap_by_bytes: u64,
}

impl ClassPlan {
    /// Whether applying this plan brings the class at or under its cap. Equal to
    /// `bytes_after <= cap_bytes`, for the reason [`crate::cleanup::CleanupPlan::cap_met`]
    /// gives: while the immune members are within the cap the size rule always reaches it,
    /// and once they are not, nothing can.
    pub fn cap_met(&self) -> bool {
        self.over_cap_by_bytes == 0
    }

    /// The ids to delete, in plan order. Handy for assertions and for logging.
    pub fn ids(&self) -> Vec<i64> {
        self.deletions.iter().map(|d| d.subject.id()).collect()
    }
}

/// What one retention pass would do to both classes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RetentionPlan {
    /// The clips half — **byte-for-byte** what [`plan_cleanup`] would decide for the same
    /// library under `policy.clips`.
    pub clips: ClassPlan,
    /// The sessions half.
    pub sessions: ClassPlan,
}

impl RetentionPlan {
    /// Whether applying this plan brings *both* classes at or under their caps.
    ///
    /// Per class, not in total: the whole point of two policies is that one class exceeding
    /// its cap is that class's problem, and reporting a combined figure would hide which
    /// class is over.
    pub fn cap_met(&self) -> bool {
        self.clips.cap_met() && self.sessions.cap_met()
    }

    /// Every deletion across both classes.
    pub fn deletions(&self) -> usize {
        self.clips.deletions.len() + self.sessions.deletions.len()
    }

    /// Whether this pass has nothing to do.
    pub fn is_empty(&self) -> bool {
        self.deletions() == 0
    }
}

/// Decide what one pass deletes from the clips library and the session store, touching
/// neither the filesystem nor the database (spec §8.1).
///
/// The rules are the ones the clips policy already implements, applied to each class
/// independently and with its own cap and age limit:
///
/// 1. **Age.** Delete every non-favourite member older than its class's `max_age_days`.
/// 2. **Size.** Evict non-favourite members least-recently-used first until the class is at
///    or under its own `max_total_bytes`. If the immune members alone exceed it, the rule
///    is suspended and the shortfall reported.
/// 3. Favourites are exempt from *both* rules, in *both* classes.
///
/// # What "least recently used" means, per class
///
/// Exactly as for clips (see [`plan_cleanup`]'s note, and the same reasoning): a row is
/// immutable, nothing here ever reads an artefact back, so the ordering is the only instant
/// in the row that is comparable across runs.
///
/// * A **clip** is aged and ordered by `clips.created_at` — the wall-clock instant it
///   entered the library, stamped by [`Store::insert_clip`], never by the caller. The
///   clips half is [`plan_cleanup`] itself, so this is not a second implementation of it.
/// * A **session** is aged and ordered by `sessions.started_at`. A session row has no
///   `created_at`, and the instant that matters for a session is when its footage begins:
///   a session that started eight days ago and ended yesterday is evicted by a seven-day
///   limit, because the footage it holds *is* eight days old however recently it stopped
///   writing. The alternative — aging by `ended_at` — would make a session immortal for as
///   long as it kept recording, which is the opposite of a retention rule.
///
/// Ties are broken by id in both classes, so a plan is deterministic for a given library
/// and `now_ms` and never depends on the order a query happened to return rows in.
///
/// # A session that is still recording
///
/// A session whose `ended_at` is NULL is in progress, and is **immune to both rules for the
/// same reason a favourite is, only more strongly**: a favourite is material the user
/// protected, while a running session's scratch directory is footage that exists nowhere
/// else yet — deleting it does not evict a copy, it destroys the only one. So:
///
/// * the age rule does not select it;
/// * the size rule does not select it;
/// * its bytes **are** counted towards the cap (they are on disk), and they count in
///   [`ClassPlan::over_cap_by_bytes`] like a favourite's, so a live recording that alone
///   exceeds the cap is reported as an unsatisfiable cap rather than paid for out of the
///   finished sessions beside it.
///
/// The plan is pure: it cannot see the process table, so "still recording" is exactly
/// `ended_at IS NULL`, and the planner and the executor both read it that way. A session
/// left running by a crash therefore stays immune until something ends it explicitly —
/// which is [`Store::end_session`]'s job, deliberately not inferred from a missing process
/// (see that method's note on the escape hatch).
///
/// # Session bytes, and the one number that can lag
///
/// The cap's arithmetic uses `sessions.size_bytes`, which [`Store::end_session`] stamps
/// when a session finishes and [`Store::set_session_size`] keeps current while it runs.
/// A recorder that does not call the latter is planning against 0 bytes for its live
/// session, so the sessions cap cannot see it until the recording stops. That is a
/// bounded, honest lag rather than a wrong eviction — an in-progress session is never an
/// eviction candidate — but it is why [`Store::set_session_size`] exists.
pub fn plan_retention(
    clips: &[Clip],
    sessions: &[Session],
    policy: &RetentionPolicy,
    now_ms: i64,
) -> RetentionPlan {
    // The clips half is the v1 policy, called rather than reimplemented: a policy that
    // evicts different clips than `plan_cleanup` would is a bug this shape cannot have.
    let flat = plan_cleanup(clips, &clip_rules(policy.clips), now_ms);
    let clips_class = ClassPlan {
        deletions: flat
            .deletions
            .iter()
            .map(|d| RetentionDeletion {
                subject: Subject::Clip(d.id),
                size_bytes: d.size_bytes,
                reasons: d.reasons,
            })
            .collect(),
        cap_bytes: flat.cap_bytes,
        bytes_after: flat.bytes_after,
        over_cap_by_bytes: flat.over_cap_by_bytes,
    };

    RetentionPlan {
        clips: clips_class,
        sessions: plan_sessions(sessions, &policy.sessions, now_ms),
    }
}

/// The clips half of a [`RetentionPolicy`] in the shape [`plan_cleanup`] takes. The only
/// place the two policy types meet.
fn clip_rules(rules: RetentionRules) -> CleanupPolicy {
    CleanupPolicy {
        max_total_bytes: rules.max_total_bytes,
        // `CleanupPolicy` counts days as u64; the retention policy uses u32 because a
        // session store's age limit is a day count, not a duration in milliseconds.
        max_age_days: rules.max_age_days as u64,
    }
}

/// The sessions class's own planner: the two rules of spec §8.1, over session rows.
///
/// The parallel with [`plan_cleanup`] is deliberate and structural — same order, same
/// marking, same unsatisfiable-cap rule — so the two read the same way and a reader who
/// knows one knows the other. See [`plan_retention`] for what a session adds: a different
/// LRU instant, whole-session units, and immunity for a session that is still recording.
fn plan_sessions(sessions: &[Session], rules: &RetentionRules, now_ms: i64) -> ClassPlan {
    let max_age_ms = (rules.max_age_days as i64).saturating_mul(MS_PER_DAY);

    // Oldest-started first, deterministic under ties. Both rules walk this one order.
    let mut ordered: Vec<&Session> = sessions.iter().collect();
    ordered.sort_by_key(|s| (s.started_at_ms, s.id));

    // One slot per session, in `ordered` order: the same "mark, never push twice" shape the
    // clips policy uses, which is what keeps a session both rules select in the plan once.
    let mut marked: Vec<Option<DeletionReasons>> = vec![None; ordered.len()];

    // A negative `size_bytes` — a hand-edited row, a writer that overflowed — counts as
    // zero rather than borrowing from the total, the same clamp `total_bytes` applies.
    let bytes = |s: &Session| s.size_bytes.max(0) as u64;

    let mut remaining: u64 = sessions.iter().map(bytes).sum();
    for (i, session) in ordered.iter().enumerate() {
        if !exempt(session) && now_ms.saturating_sub(session.started_at_ms) > max_age_ms {
            marked[i] = Some(DeletionReasons { too_old: true, over_cap: false });
            remaining = remaining.saturating_sub(bytes(session));
        }
    }

    // Is the cap reachable at all? Only while the bytes nothing may evict are within it.
    let immune_bytes: u64 = sessions.iter().filter(|s| exempt(s)).map(bytes).sum();
    let over_cap_by_bytes = immune_bytes.saturating_sub(rules.max_total_bytes);

    if over_cap_by_bytes == 0 {
        for (i, session) in ordered.iter().enumerate() {
            if remaining <= rules.max_total_bytes {
                break;
            }
            if exempt(session) {
                continue;
            }
            match &mut marked[i] {
                Some(reasons) => reasons.over_cap = true,
                None => {
                    marked[i] = Some(DeletionReasons { too_old: false, over_cap: true });
                    remaining = remaining.saturating_sub(bytes(session));
                }
            }
        }
    }

    let deletions = ordered
        .iter()
        .zip(&marked)
        .filter_map(|(session, reasons)| {
            reasons.map(|reasons| RetentionDeletion {
                subject: Subject::Session(session.id),
                size_bytes: bytes(session),
                reasons,
            })
        })
        .collect();

    ClassPlan {
        deletions,
        cap_bytes: rules.max_total_bytes,
        bytes_after: remaining,
        over_cap_by_bytes,
    }
}

/// Whether a session is out of the retention rules' reach: favourited, or still recording.
fn exempt(session: &Session) -> bool {
    session.favourite || session.ended_at_ms.is_none()
}

/// What one retention pass actually did to one class, and to the disk under it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassOutcome {
    /// Rows this pass deleted and committed.
    pub deleted: usize,
    /// Planned deletions that were refused or failed; each is logged where it happens.
    /// Counted rather than propagated, because one unreadable row must not abandon the
    /// rest of a pass.
    pub failed: usize,
    /// Bytes this pass actually removed from the disk, measured file by file as they were
    /// removed — not forecast from the rows. A row that disagrees with its file (a
    /// truncated write, a re-encode, a hand-edited index) therefore reports what went away.
    pub bytes_reclaimed: u64,
    /// Paths a *deleted* row named whose removal failed: the file is on disk with no row
    /// naming it, so a later sweep (or the user) has to deal with it.
    pub orphaned: Vec<PathBuf>,
    /// Paths a deleted row named that were already absent from the disk. The end state is
    /// right — no row, no file — but a row pointing at nothing is worth reporting.
    pub already_missing: Vec<PathBuf>,
    /// Paths this pass deliberately refused to remove, because removing them could destroy
    /// something that is not this class's to destroy — a scratch directory that is a
    /// symlink, a bare relative name, a filesystem root. Never silently skipped: a refused
    /// path stays on disk, the row is already gone, and the caller is told.
    pub refused: Vec<PathBuf>,
    /// Total size of the class after the pass, re-read from the database.
    pub bytes_after: u64,
    /// Whether the class is at or under the plan's cap **after** the pass, measured rather
    /// than forecast.
    pub cap_met: bool,
}

/// What one retention pass did to both classes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionOutcome {
    pub clips: ClassOutcome,
    pub sessions: ClassOutcome,
}

impl RetentionOutcome {
    /// Rows deleted across both classes.
    pub fn deleted(&self) -> usize {
        self.clips.deleted + self.sessions.deleted
    }

    /// Bytes removed from the disk across both classes.
    pub fn bytes_reclaimed(&self) -> u64 {
        self.clips.bytes_reclaimed + self.sessions.bytes_reclaimed
    }

    /// Whether both classes ended at or under their own cap.
    pub fn cap_met(&self) -> bool {
        self.clips.cap_met && self.sessions.cap_met
    }

    /// Every path left behind that a log line should name.
    pub fn leftovers(&self) -> impl Iterator<Item = &PathBuf> {
        self.clips
            .orphaned
            .iter()
            .chain(&self.clips.refused)
            .chain(&self.sessions.orphaned)
            .chain(&self.sessions.refused)
    }
}

/// Apply a retention plan: for every deletion, delete the row and commit, then remove what
/// that committed delete handed back (spec §8.2).
///
/// The ordering is the same safety property [`crate::cleanup::execute_cleanup`] carries,
/// and the clips half is the same sequence of calls: delete the row via
/// [`Store::delete_clip_returning_path`], which returns a path only when its own `DELETE`
/// removed a row, then unlink *that* path and nothing else. A file is never removed on a
/// plan's say-so; it is removed because a committed row named it.
///
/// For a session the row delete is [`Store::delete_session_returning_paths`], and the
/// paths it hands back are two different things:
///
/// * the **final file**, removed like a clip's file, with a failed or already-missing
///   removal reported exactly as one is for a clip;
/// * the **scratch directory**, removed as a tree. That is the most destructive single
///   operation in this crate, so it is guarded rather than trusted: the path must look
///   like a directory this application could have created (non-empty, with a final path
///   component, and a real directory rather than a symlink) before anything inside it is
///   touched. See [`remove_scratch_dir`]. A refused path is reported in
///   [`ClassOutcome::refused`] and left exactly as it is.
///
/// A session that is still recording cannot reach the removal code at all: its row delete
/// is refused by the store (see [`Store::delete_session_returning_paths`]), which this
/// counts as a failed deletion and logs. The planner never selects one, so that path means
/// a plan built by hand or held across the moment the session started — and both are worth
/// a warning rather than a crash.
///
/// Errors are per-row: one failed row is counted in [`ClassOutcome::failed`] and logged
/// instead of ending the pass. The returned `Err` is reserved for the final re-read of the
/// store.
pub fn execute_retention(store: &Store, plan: &RetentionPlan) -> Result<RetentionOutcome> {
    let mut outcome = RetentionOutcome::default();

    // Clips first, then sessions: the order the plan lists them in, and the order that
    // leaves a session's links to its clips already detached by the time the clips go.
    for entry in &plan.clips.deletions {
        let Subject::Clip(id) = entry.subject else {
            tracing::warn!(
                "retention: the plan lists {} #{} under clips; skipping it",
                entry.subject.class(),
                entry.subject.id()
            );
            outcome.clips.failed += 1;
            continue;
        };
        match store.delete_clip_returning_path(id) {
            Ok(Some(path)) => {
                // That DELETE ran in autocommit: it is committed before this line.
                outcome.clips.deleted += 1;
                let path = PathBuf::from(path);
                record(&mut outcome.clips, &path, unlink(&path), entry.size_bytes);
            }
            Ok(None) => {
                tracing::debug!(
                    "retention: clip #{id} had no row left to delete; nothing was removed"
                );
            }
            Err(err) => {
                tracing::warn!("retention: could not delete the row for clip #{id}: {err:#}");
                outcome.clips.failed += 1;
            }
        }
    }

    for entry in &plan.sessions.deletions {
        let Subject::Session(id) = entry.subject else {
            tracing::warn!(
                "retention: the plan lists {} #{} under sessions; skipping it",
                entry.subject.class(),
                entry.subject.id()
            );
            outcome.sessions.failed += 1;
            continue;
        };
        let paths = match store.delete_session_returning_paths(id) {
            Ok(Some(paths)) => paths,
            Ok(None) => {
                tracing::debug!(
                    "retention: session #{id} had no row left to delete; nothing was removed"
                );
                continue;
            }
            Err(err) => {
                // The expected reason here is the store refusing a session that is still
                // recording; any other is a real failure. Either way the session is left
                // whole, which is the safe direction.
                tracing::warn!("retention: session #{id} was not deleted: {err:#}");
                outcome.sessions.failed += 1;
                continue;
            }
        };
        // Committed before this line, as for a clip.
        outcome.sessions.deleted += 1;

        if let Some(final_path) = paths.final_path.as_deref() {
            let final_path = PathBuf::from(final_path);
            record(&mut outcome.sessions, &final_path, unlink(&final_path), entry.size_bytes);
        }
        let scratch = PathBuf::from(&paths.scratch_dir);
        record(&mut outcome.sessions, &scratch, remove_scratch_dir(&scratch), entry.size_bytes);
    }

    outcome.clips.bytes_after = store.total_bytes()?;
    outcome.clips.cap_met = outcome.clips.bytes_after <= plan.clips.cap_bytes;
    outcome.sessions.bytes_after = store.total_session_bytes()?;
    outcome.sessions.cap_met = outcome.sessions.bytes_after <= plan.sessions.cap_bytes;
    Ok(outcome)
}

/// What one attempt to remove a path did. One type for files and for trees, so the
/// bookkeeping is written once and every removal reports itself the same way.
///
/// Deliberately not `PartialEq`: `Removal::Failed` carries an `io::Error`, which does not
/// compare, and the failures are reported rather than compared. Tests match on the variant.
#[derive(Debug)]
enum Removal {
    /// Removed. `u64` is what it actually held, measured from the disk first.
    Removed(u64),
    /// Not there in the first place.
    Missing,
    /// Deliberately not removed, for the stated reason. The path is untouched.
    Refused(&'static str),
    Failed(std::io::Error),
}

/// Fold one removal attempt into a class's outcome, and say where it happened.
///
/// Every non-removal lands in one of the three path lists, so an outcome is a complete
/// account of what a pass could not remove: a path is never dropped because the log line
/// describing it went to a subscriber nobody is reading.
///
/// `planned_bytes` is used only for the debug line that notices a file was smaller or
/// larger than the row claimed: the bytes counted into `bytes_reclaimed` are the measured
/// ones, so the total is what the disk lost rather than what the index believed it held.
fn record(outcome: &mut ClassOutcome, path: &Path, removal: Removal, planned_bytes: u64) {
    match removal {
        Removal::Removed(actual) => {
            outcome.bytes_reclaimed += actual;
            if actual != planned_bytes {
                tracing::debug!(
                    "retention: removed {actual} bytes from {} where the row claimed \
                     {planned_bytes}",
                    path.display()
                );
            }
        }
        Removal::Missing => {
            tracing::warn!(
                "retention: a deleted row named {} and it was already gone — the index and the \
                 disk disagreed about it",
                path.display()
            );
            outcome.already_missing.push(path.to_path_buf());
        }
        Removal::Refused(reason) => {
            tracing::warn!(
                "retention: refused to remove {} ({reason}); it is left exactly as it is",
                path.display()
            );
            outcome.refused.push(path.to_path_buf());
        }
        Removal::Failed(err) => {
            tracing::warn!(
                "retention: a deleted row named {} and it could not be removed ({err}); it is \
                 orphaned on disk and needs a sweep",
                path.display()
            );
            outcome.orphaned.push(path.to_path_buf());
        }
    }
}

/// Remove one regular file, measuring what actually went away.
///
/// The measurement is taken from the disk before the unlink, not from the row: this is the
/// number a user is shown, and a row that disagrees with its file would make the row's
/// number a lie. `NotFound` is reported as such rather than as a silent zero, because a row
/// pointing at nothing is worth knowing about even when the end state is right.
fn unlink(path: &Path) -> Removal {
    let bytes = match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.is_file() {
                meta.len()
            } else {
                // A directory (or a symlink) where a file was expected: `remove_file`
                // below will refuse it, and there are no bytes of *this* path to count.
                0
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Removal::Missing,
        Err(err) => return Removal::Failed(err),
    };
    match std::fs::remove_file(path) {
        Ok(()) => Removal::Removed(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Removal::Missing,
        Err(err) => Removal::Failed(err),
    }
}

/// Remove a session's scratch directory and everything in it, measuring the tree first.
///
/// This is the one place this crate removes something it did not itself write, and the path
/// comes out of a TEXT column that a bug or a hand-edited index could hold anything in.
/// `remove_dir_all` given `/`, `.`, `..` or a bare relative name is a catastrophe, so the
/// path has to look like a directory this application could have created before a single
/// entry is touched:
///
/// * non-empty and with a final path component, which rules out `.`, `..` and every
///   filesystem root;
/// * a real directory, not a symlink — a symlink would send the walk wherever it points,
///   which is not what the row named — and not a plain file.
///
/// Anything else comes back as [`Removal::Refused`] with the reason, and the caller reports
/// it. A path that is simply gone is [`Removal::Missing`], which is an ordinary end state
/// for a scratch directory (a session-mode recorder may remove its own scratch directory
/// once the final file exists) and not a refusal.
///
/// The tree is measured before it is removed, and a tree that cannot be measured is *not*
/// removed: an unreadable directory is a question for a human, and being unable to say what
/// a removal freed is a good sign it should not happen.
fn remove_scratch_dir(path: &Path) -> Removal {
    if path.as_os_str().is_empty() || path.file_name().is_none() {
        return Removal::Refused(
            "no final path component: it names a filesystem root, '.' or '..'",
        );
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Removal::Missing,
        Err(err) => return Removal::Failed(err),
    };
    if meta.is_symlink() {
        return Removal::Refused("it is a symlink, and removing it would reach past the path");
    }
    if !meta.is_dir() {
        return Removal::Refused("it is not a directory");
    }
    let bytes = match tree_bytes(path) {
        Ok(bytes) => bytes,
        Err(err) => return Removal::Failed(err),
    };
    match std::fs::remove_dir_all(path) {
        Ok(()) => Removal::Removed(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Removal::Missing,
        Err(err) => Removal::Failed(err),
    }
}

/// The bytes a directory tree holds: every regular file in it, to any depth.
///
/// Symlinks count as zero and are never followed — what a scratch directory holds is what
/// this application wrote into it, and a symlink is a path to somewhere else, not bytes.
fn tree_bytes(path: &Path) -> std::io::Result<u64> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_symlink() {
        return Ok(0);
    }
    if meta.is_file() {
        return Ok(meta.len());
    }
    if !meta.is_dir() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in std::fs::read_dir(path)? {
        total += tree_bytes(&entry?.path())?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SESSION_MODE_BUFFER, SESSION_MODE_SESSION};
    use std::path::PathBuf;

    const DAY: i64 = MS_PER_DAY;

    fn rules(max_total_bytes: u64, max_age_days: u32) -> RetentionRules {
        RetentionRules { max_total_bytes, max_age_days }
    }

    /// A policy whose clips half and sessions half can be set independently — the point of
    /// the whole module, and what every independence test varies.
    fn policy(clips: RetentionRules, sessions: RetentionRules) -> RetentionPolicy {
        RetentionPolicy { clips, sessions }
    }

    /// A cap nothing reaches and an age nothing exceeds: the plan must be a no-op.
    fn generous() -> RetentionRules {
        rules(u64::MAX, 3_650)
    }

    fn clip(id: i64, created_at_ms: i64, size_bytes: u64, favourite: bool) -> Clip {
        Clip {
            id,
            path: PathBuf::from(format!("/clips/{id}.mp4")),
            started_at_ms: 0,
            duration_ms: 1_000,
            size_bytes,
            codec: "h264_nvenc".into(),
            favourite,
            created_at_ms,
        }
    }

    /// A finished session as `Store::list_sessions` would hand it to `plan_retention`.
    fn session(id: i64, started_at_ms: i64, size_bytes: u64, favourite: bool) -> Session {
        Session {
            id,
            game: Some("League of Legends".into()),
            mode: SESSION_MODE_SESSION.into(),
            started_at_ms,
            // These fixtures describe sessions that started against an empty scratch
            // directory, which is the ordinary case and the one every pre-v3 row means.
            media_epoch_ms: 0,
            ended_at_ms: Some(started_at_ms + 60_000),
            scratch_dir: format!("/scratch/{id}"),
            final_path: Some(format!("/sessions/{id}.mp4")),
            size_bytes: size_bytes as i64,
            // A minute of media, matching the minute between `started_at` and `ended_at`
            // above: these fixtures describe sessions whose encoder kept up, so the media
            // length and the wall-clock window agree. (That is not generally true — see
            // `Session::duration_ms` — but it is the case a retention planner sees.)
            duration_ms: 60_000,
            favourite,
        }
    }

    /// The same row, still recording: `ended_at` is NULL and there is no final file yet.
    fn running(id: i64, started_at_ms: i64, size_bytes: u64) -> Session {
        Session {
            ended_at_ms: None,
            final_path: None,
            ..session(id, started_at_ms, size_bytes, false)
        }
    }

    #[test]
    fn a_store_within_both_classes_rules_plans_nothing() {
        let now = 10 * DAY;
        let clips = vec![clip(1, now - 2 * DAY, 1_000, false)];
        let sessions = vec![session(1, now - DAY, 5_000, false)];

        let plan = plan_retention(&clips, &sessions, &policy(generous(), generous()), now);

        assert!(plan.is_empty());
        assert!(plan.cap_met());
        assert_eq!(plan.clips.bytes_after, 1_000);
        assert_eq!(plan.sessions.bytes_after, 5_000);
    }

    #[test]
    fn the_clips_half_is_exactly_what_the_flat_policy_would_decide() {
        // The one property that makes two entry points safe to have: the clips class is
        // `plan_cleanup` itself, converted, so a retention pass and a cleanup pass cannot
        // disagree about the same clips library.
        let now = 1_000 * DAY;
        let clips = vec![
            clip(1, now - 40 * DAY, 100, false), // over age, and the first eviction candidate
            clip(2, now - 2 * DAY, 100, false),
            clip(3, now - 2 * DAY, 5, true),
        ];
        let flat = plan_cleanup(&clips, &clip_rules(rules(10, 7)), now);
        let plan = plan_retention(&clips, &[], &policy(rules(10, 7), generous()), now);

        assert_eq!(plan.clips.ids(), flat.ids());
        assert_eq!(plan.clips.bytes_after, flat.bytes_after);
        assert_eq!(plan.clips.over_cap_by_bytes, flat.over_cap_by_bytes);
        assert_eq!(plan.clips.cap_bytes, flat.cap_bytes);
        assert_eq!(
            plan.clips.deletions[0],
            RetentionDeletion {
                subject: Subject::Clip(1),
                size_bytes: 100,
                reasons: DeletionReasons { too_old: true, over_cap: true },
            },
            "the reasons travel with the deletion, both of them"
        );
    }

    #[test]
    fn the_two_classes_are_independent_in_both_directions() {
        let now = 1_000 * DAY;
        let clips = vec![clip(1, now - 40 * DAY, 100, false)];
        let sessions = vec![session(7, now - 40 * DAY, 900, false)];

        // A sessions cap of zero must not evict a clip, however small the clips library.
        let plan = plan_retention(&clips, &sessions, &policy(generous(), rules(0, 3_650)), now);
        assert_eq!(plan.sessions.ids(), vec![7]);
        assert!(plan.clips.deletions.is_empty(), "a session cap is not a clip cap");

        // And a clips cap of zero must not evict a session.
        let plan = plan_retention(&clips, &sessions, &policy(rules(0, 3_650), generous()), now);
        assert_eq!(plan.clips.ids(), vec![1]);
        assert!(plan.sessions.deletions.is_empty(), "a clip cap is not a session cap");
    }

    #[test]
    fn the_sessions_age_rule_uses_started_at_and_not_ended_at() {
        let now = 100 * DAY;
        // Ended yesterday, but it started eight days ago: the footage is eight days old.
        let sessions = vec![session(1, now - 8 * DAY, 10, false)];

        let plan = plan_retention(&[], &sessions, &policy(generous(), rules(u64::MAX, 7)), now);

        assert_eq!(plan.sessions.ids(), vec![1]);
        assert!(plan.sessions.deletions[0].reasons.too_old);
        assert!(!plan.sessions.deletions[0].reasons.over_cap);
    }

    #[test]
    fn the_sessions_age_limit_is_a_strict_threshold() {
        let now = 3 * DAY;
        let sessions = vec![
            session(1, now - DAY, 1, false), // exactly one day old, one day limit: kept
            session(2, now - DAY - 1, 1, false), // one ms older: evicted
        ];

        let plan = plan_retention(&[], &sessions, &policy(generous(), rules(u64::MAX, 1)), now);

        assert_eq!(plan.sessions.ids(), vec![2]);
    }

    #[test]
    fn the_sessions_size_rule_evicts_oldest_started_first() {
        let now = 1_000 * DAY;
        // Handed in a deliberately scrambled order: the plan must not depend on it.
        let sessions = vec![
            session(3, now - 3 * DAY, 100, false),
            session(1, now - 9 * DAY, 100, false),
            session(2, now - 6 * DAY, 100, false),
        ];

        let plan =
            plan_retention(&[], &sessions, &policy(generous(), rules(150, 3_650)), now);

        assert_eq!(plan.sessions.ids(), vec![1, 2], "oldest started first, until under the cap");
        assert_eq!(plan.sessions.bytes_after, 100, "100 bytes is at the cap, not over it");
        assert!(plan.cap_met());
        assert!(plan.sessions.deletions.iter().all(|d| d.reasons.over_cap && !d.reasons.too_old));
    }

    #[test]
    fn a_session_both_rules_select_appears_once_with_both_reasons() {
        let now = 1_000 * DAY;
        let sessions = vec![
            session(1, now - 40 * DAY, 100, false), // over age, and the oldest candidate
            session(2, now - DAY, 100, false),
            session(3, now - DAY, 5, true),
        ];

        let plan = plan_retention(&[], &sessions, &policy(generous(), rules(10, 7)), now);

        assert_eq!(plan.sessions.ids(), vec![1, 2], "one entry per session");
        assert_eq!(
            plan.sessions.deletions[0].reasons,
            DeletionReasons { too_old: true, over_cap: true }
        );
        assert_eq!(plan.sessions.bytes_after, 5, "bytes counted once, however selected");
    }

    #[test]
    fn favourites_are_immune_in_both_classes_and_by_both_rules() {
        let now = 1_000 * DAY;
        // Clips: an ancient favourite and a favourite that alone exceeds the clip cap.
        let clips = vec![
            clip(1, now - 90 * DAY, 4_000, true),
            clip(2, now - 90 * DAY, 6_000, true),
            clip(3, now - DAY, 100, false),
        ];
        // Sessions: the same two situations.
        let sessions = vec![
            session(1, now - 90 * DAY, 4_000, true),
            session(2, now - 90 * DAY, 6_000, true),
            session(3, now - DAY, 100, false),
        ];

        let plan = plan_retention(&clips, &sessions, &policy(rules(10, 1), rules(10, 1)), now);

        assert_eq!(
            plan.clips.ids(),
            Vec::<i64>::new(),
            "no clip is evicted for a cap it cannot meet"
        );
        assert_eq!(plan.sessions.ids(), Vec::<i64>::new(), "and no session either");
        assert_eq!(plan.clips.over_cap_by_bytes, 9_990);
        assert_eq!(plan.sessions.over_cap_by_bytes, 9_990);
        assert!(!plan.cap_met(), "and both shortfalls are reported rather than paid for");
        assert_eq!(plan.clips.bytes_after, 10_100);
        assert_eq!(plan.sessions.bytes_after, 10_100);
    }

    #[test]
    fn a_session_that_is_still_recording_is_immune_to_both_rules() {
        let now = 1_000 * DAY;
        let sessions = vec![
            running(1, now - 90 * DAY, 100),    // ancient, huge, and recording
            session(2, now - 90 * DAY, 10_000, true), // an ancient favourite
            session(3, now - DAY, 100, false),  // recent and small: nothing wants it
        ];

        // Every rule violated as hard as it can be: 1 day of age, 10 bytes of cap.
        let plan = plan_retention(&[], &sessions, &policy(generous(), rules(10, 1)), now);

        assert!(
            plan.sessions.deletions.is_empty(),
            "a running session is not evictable, and the cap it breaks is unsatisfiable: got {:?}",
            plan.sessions.ids()
        );
        assert_eq!(
            plan.sessions.over_cap_by_bytes, 10_090,
            "the immutable bytes — the running session plus the favourite — are the shortfall"
        );
        assert!(!plan.cap_met());
        assert_eq!(plan.sessions.bytes_after, 10_200, "nothing was deleted");
    }

    #[test]
    fn an_unsatisfiable_session_cap_does_not_suspend_the_age_rule() {
        let now = 1_000 * DAY;
        let sessions = vec![
            running(1, now - 90 * DAY, 200),           // recording: exempt
            session(2, now - 30 * DAY, 400, false),    // ancient, finished: the age rule's job
            session(3, now - DAY, 100, false),
        ];

        let plan = plan_retention(&[], &sessions, &policy(generous(), rules(10, 7)), now);

        assert_eq!(plan.sessions.ids(), vec![2], "the age rule is independent of the cap");
        assert!(plan.sessions.deletions[0].reasons.too_old);
        assert!(!plan.cap_met());
        assert_eq!(plan.sessions.over_cap_by_bytes, 190, "200 of running session against 10");
        assert_eq!(plan.sessions.bytes_after, 300);
    }

    #[test]
    fn a_negative_size_counts_as_zero_rather_than_underflowing_the_total() {
        let now = 1_000 * DAY;
        let sessions = vec![
            Session { size_bytes: -5, ..session(1, now - 40 * DAY, 0, false) },
            session(2, now - 39 * DAY, 100, false),
        ];

        let plan = plan_retention(&[], &sessions, &policy(generous(), rules(0, 3_650)), now);

        assert_eq!(plan.sessions.ids(), vec![1, 2]);
        assert_eq!(plan.sessions.bytes_after, 0, "a corrupt size never subtracts from the total");
    }

    #[test]
    fn a_session_started_in_the_future_is_never_too_old() {
        // A clock that stepped backwards between two runs must not make the newest session
        // look like the oldest — the same guard the clips policy carries.
        let now = 10 * DAY;
        let sessions = vec![session(1, now + 5 * DAY, 1_000, false)];

        let plan = plan_retention(&[], &sessions, &policy(generous(), rules(u64::MAX, 1)), now);

        assert!(plan.sessions.deletions.is_empty());
        assert_eq!(plan.sessions.bytes_after, 1_000);
    }

    // ------------------------------------------------------------ the executor

    /// A store the executor can run against, with the v2 columns — used for the cases where
    /// the plan is built by hand rather than by `plan_retention`.
    fn store() -> Store {
        let store = Store::open_in_memory().expect("an in-memory store");
        store.migrate().expect("migrate");
        store
    }

    #[test]
    fn the_executor_skips_a_plan_entry_that_is_the_wrong_class() {
        // Defensive: a plan is `plan_retention`'s to build, and it cannot produce this. An
        // executor that trusted it would delete a session when asked about a clip.
        let store = store();
        let session_id = store
            .start_session(None, SESSION_MODE_BUFFER, 0, "/scratch/keep", 0)
            .expect("open a session");
        store.end_session(session_id, 1, None, 0, 0).expect("end it");

        let plan = RetentionPlan {
            clips: ClassPlan {
                deletions: vec![RetentionDeletion {
                    subject: Subject::Session(session_id),
                    size_bytes: 0,
                    reasons: DeletionReasons { too_old: true, over_cap: false },
                }],
                cap_bytes: 0,
                bytes_after: 0,
                over_cap_by_bytes: 0,
            },
            sessions: ClassPlan::default(),
        };

        let outcome = execute_retention(&store, &plan).expect("the pass completes");

        assert_eq!(outcome.clips.deleted, 0);
        assert_eq!(outcome.clips.failed, 1, "the mis-classed entry is counted, not obeyed");
        assert!(store.get_session(session_id).expect("read").is_some(), "the session survives");
    }

    #[test]
    fn the_executor_refuses_a_running_session_even_when_a_plan_names_it() {
        // The planner never selects one. A plan that does — built by hand, or held across
        // the moment the session started — must still not be a way to lose a recording.
        let store = store();
        let running_id = store
            .start_session(Some("League of Legends"), SESSION_MODE_SESSION, 1_000, "/scratch/live", 0)
            .expect("open a session");

        let plan = RetentionPlan {
            clips: ClassPlan::default(),
            sessions: ClassPlan {
                deletions: vec![RetentionDeletion {
                    subject: Subject::Session(running_id),
                    size_bytes: 0,
                    reasons: DeletionReasons { too_old: true, over_cap: false },
                }],
                cap_bytes: u64::MAX,
                bytes_after: 0,
                over_cap_by_bytes: 0,
            },
        };

        let outcome = execute_retention(&store, &plan).expect("the pass completes");

        assert_eq!(outcome.sessions.deleted, 0);
        assert_eq!(outcome.sessions.failed, 1, "the refusal is reported, not swallowed");
        assert_eq!(outcome.sessions.bytes_reclaimed, 0, "and nothing was removed");
        assert!(
            store.get_session(running_id).expect("read").is_some(),
            "the recording's row is still there"
        );
    }

    #[test]
    fn a_plan_id_whose_row_is_already_gone_removes_nothing() {
        // The property the deletion ordering exists for: with no committed row delete,
        // there is no authority to remove anything.
        let store = store();
        let id = store
            .start_session(None, SESSION_MODE_BUFFER, 0, "/scratch/absent", 0)
            .expect("open a session");
        store.end_session(id, 1, None, 0, 0).expect("end it");
        let plan = RetentionPlan {
            clips: ClassPlan::default(),
            sessions: ClassPlan {
                deletions: vec![RetentionDeletion {
                    subject: Subject::Session(id),
                    size_bytes: 0,
                    reasons: DeletionReasons { too_old: true, over_cap: false },
                }],
                cap_bytes: u64::MAX,
                bytes_after: 0,
                over_cap_by_bytes: 0,
            },
        };
        assert!(store.delete_session_returning_paths(id).expect("delete it").is_some());

        let outcome = execute_retention(&store, &plan).expect("the pass completes");

        assert_eq!(outcome.sessions.deleted, 0);
        assert_eq!(outcome.sessions.failed, 0, "a row already gone is not a failure");
        assert!(outcome.sessions.already_missing.is_empty());
        assert!(outcome.sessions.orphaned.is_empty());
        assert!(outcome.sessions.refused.is_empty());
    }

    // ------------------------------------------------- removal guards (no temp dirs)

    #[test]
    fn a_scratch_directory_with_no_final_component_is_refused() {
        // These are the paths that make `remove_dir_all` a catastrophe. The guard is the
        // reason this module can hold a path from a TEXT column at all.
        for path in ["", ".", "..", "/"] {
            assert!(
                matches!(
                    remove_scratch_dir(Path::new(path)),
                    Removal::Refused(_)
                ),
                "{path:?} must be refused, never recursed into"
            );
        }
    }

    #[test]
    fn a_file_where_a_scratch_directory_should_be_is_refused_not_unlinked() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"bytes").expect("write it");

        match remove_scratch_dir(&file) {
            Removal::Refused(reason) => assert!(reason.contains("not a directory"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(file.is_file(), "and the file it refused is untouched");
    }

    #[test]
    fn a_symlinked_scratch_directory_is_refused_rather_than_followed() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("create the target");
        std::fs::write(elsewhere.join("keep.bin"), b"not ours").expect("write into it");
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&elsewhere, &link).expect("symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&elsewhere, &link).expect("symlink");

        match remove_scratch_dir(&link) {
            Removal::Refused(reason) => assert!(reason.contains("symlink"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(elsewhere.join("keep.bin").is_file(), "the tree it pointed at is untouched");
    }

    #[test]
    fn a_scratch_directory_that_is_already_gone_is_missing_not_an_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let removal = remove_scratch_dir(&dir.path().join("never-existed"));
        assert!(matches!(removal, Removal::Missing), "got {removal:?}");
    }

    #[test]
    fn removing_a_scratch_directory_reports_the_bytes_the_tree_really_held() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let scratch = dir.path().join("scratch-1");
        std::fs::create_dir_all(scratch.join("nested")).expect("create the tree");
        std::fs::write(scratch.join("seg-1.mkv"), vec![0u8; 400]).expect("segment");
        std::fs::write(scratch.join("nested").join("seg-2.mkv"), vec![0u8; 300]).expect("segment");

        // What is really on the disk, read back file by file: the measurement is checked
        // against the files the fixture wrote, not against a number the code produced.
        let seg_1 = std::fs::metadata(scratch.join("seg-1.mkv")).expect("stat").len();
        let seg_2 =
            std::fs::metadata(scratch.join("nested").join("seg-2.mkv")).expect("stat").len();
        assert_eq!(seg_1 + seg_2, 700, "the fixture wrote 700 bytes into the tree");

        match remove_scratch_dir(&scratch) {
            Removal::Removed(bytes) => {
                assert_eq!(bytes, seg_1 + seg_2, "measured from the tree, not assumed")
            }
            other => panic!("expected a removal, got {other:?}"),
        }
        assert!(!scratch.exists(), "and the tree is gone");
    }

    #[test]
    fn a_missing_file_is_reported_and_a_directory_is_not_unlinked_as_one() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let missing = unlink(&dir.path().join("gone.mp4"));
        assert!(matches!(missing, Removal::Missing), "got {missing:?}");

        let file = dir.path().join("clip.mp4");
        std::fs::write(&file, vec![0u8; 123]).expect("write the clip");
        let removed = unlink(&file);
        assert!(matches!(removed, Removal::Removed(123)), "the bytes it really held: {removed:?}");
        assert!(!file.exists());

        // A directory where a file was expected: `remove_file` refuses it, and the caller
        // reports an orphan rather than recursively deleting something it never indexed.
        let as_dir = dir.path().join("surprise.mp4");
        std::fs::create_dir_all(&as_dir).expect("create the directory");
        std::fs::write(as_dir.join("inside.bin"), b"still here").expect("write into it");
        assert!(matches!(unlink(&as_dir), Removal::Failed(_)));
        assert!(as_dir.join("inside.bin").is_file(), "nothing was reached inside it");
    }
}
