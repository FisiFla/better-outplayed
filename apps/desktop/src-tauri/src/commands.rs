//! The desktop shell's command layer — everything the IPC commands actually do.
//!
//! # Why the logic lives here and not in the `#[tauri::command]` functions
//!
//! Every function in this module is a plain function over explicit dependencies: a
//! [`Store`], an optional [`FfmpegBinaries`], a set of paths, a storage policy. None of
//! them takes a `State`, an `AppHandle` or a window, and none of them is `async`. The
//! `#[tauri::command]` wrappers in `lib.rs` resolve the application state and delegate,
//! and contain no logic of their own.
//!
//! That split is the whole point of this file. It is what lets the suite below drive the
//! real command bodies against a **real** SQLite store and **real** ffmpeg-produced files
//! on disk, with no Tauri runtime and no window — a window this repository's development
//! host has no display for, and which a test suite must never open anyway (spec §12).
//!
//! # What these commands do not do
//!
//! No command here captures the screen, enumerates windows or synthesises input, and none
//! of them ever re-encodes: `trim_clip` is [`localplay_media::edit::trim_lossless`], a
//! stream copy (`-c copy`), and the UI says so. The `events` table is not read — game
//! event markers are Phase 4 and the timeline draws none (see `src/lib/components/
//! Timeline.svelte`).

use crate::config::RecordingConfig;
#[cfg(test)]
use localplay_capture::stub::StubConfig;
use localplay_media::edit::{thumbnail as ffmpeg_thumbnail, trim_lossless};
use localplay_media::probe::MediaInfo;
use localplay_media::FfmpegBinaries;
use localplay_recorder::{Recorder, RecorderConfig, RecorderStatus, Sources};
use localplay_store::cleanup::{plan_cleanup, CleanupPolicy};
use localplay_store::{Clip, NewClip, Session, SessionEvent, Store};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// The app-data-relative directories the shell reads and writes.
///
/// `clips_dir` is where the recorder writes clips and where a trim's new file goes — next
/// to its parent, which is what makes a trimmed clip visible to the CLI's storage policy
/// without any extra wiring. `thumbnails_dir` is a cache: nothing but this app reads it,
/// and losing it costs one ffmpeg run per clip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub clips_dir: PathBuf,
    pub thumbnails_dir: PathBuf,
    pub db_path: PathBuf,
}

impl AppPaths {
    /// Resolve the paths for `app_data_dir` under the `[storage]` settings.
    ///
    /// An empty `clips_dir` means the `clips` directory under the application data
    /// directory — the same rule the CLI applies, which is what makes the two binaries
    /// operate on one library instead of two.
    pub fn resolve(app_data_dir: &Path, clips_dir_setting: &str) -> Self {
        let clips_dir = if clips_dir_setting.is_empty() {
            app_data_dir.join("clips")
        } else {
            PathBuf::from(clips_dir_setting)
        };
        Self {
            clips_dir,
            thumbnails_dir: app_data_dir.join("thumbnails"),
            db_path: app_data_dir.join("localplay.db"),
        }
    }

    /// The directories the asset protocol must be allowed to serve.
    ///
    /// Playback (`<video>`) only ever requests paths under `clips_dir`; the thumbnails
    /// directory is here because a cached JPEG is served to an `<img>` the same way. Both
    /// are handed to the runtime scope in `lib.rs`; the static scope in `tauri.conf.json`
    /// covers the default layout only, and a configured `storage.clips_dir` is exactly the
    /// case that needs the runtime one.
    pub fn asset_roots(&self) -> [&Path; 2] {
        [self.clips_dir.as_path(), self.thumbnails_dir.as_path()]
    }
}

/// Everything a command needs, passed explicitly.
///
/// `bins` is optional because ffmpeg is discovered at startup and a missing ffmpeg must
/// not stop the shell from opening: listing clips and reading the storage policy need no
/// ffmpeg at all, and the two commands that do need it fail with
/// [`ErrorCode::FfmpegUnavailable`] and the discovery error rather than taking the window
/// down with them.
#[derive(Clone, Copy)]
pub struct Deps<'a> {
    pub store: &'a Store,
    pub bins: Option<&'a FfmpegBinaries>,
    pub paths: &'a AppPaths,
    pub storage: &'a StorageConfigView,
    /// Problems found at startup that the UI should surface rather than hide.
    pub warnings: &'a [String],
}

impl<'a> Deps<'a> {
    fn binaries(&self) -> Result<&'a FfmpegBinaries, CommandError> {
        self.bins.ok_or_else(|| {
            CommandError::new(
                ErrorCode::FfmpegUnavailable,
                "ffmpeg was not found, so clips cannot be trimmed or thumbnailed. \
                 Install ffmpeg on PATH or place the sidecar binaries next to the \
                 application.",
            )
        })
    }
}

/// The storage policy as the commands see it (spec §8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageConfigView {
    pub max_total_bytes: u64,
    pub max_age_days: u64,
}

impl From<&crate::config::StorageConfig> for StorageConfigView {
    fn from(cfg: &crate::config::StorageConfig) -> Self {
        Self { max_total_bytes: cfg.max_total_bytes, max_age_days: cfg.max_age_days }
    }
}

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

/// The class of a failure. The frontend switches on this; `message` is for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No clip with that id is in the index.
    ClipNotFound,
    /// No recording session with that id is in the index.
    SessionNotFound,
    /// The session is still recording. Its scratch directory is the only copy of the
    /// footage, so there is nothing safe to cut from it or delete yet.
    SessionStillRecording,
    /// The range has no length (`end <= start`). Rejected before ffmpeg is ever spawned.
    InvalidRange,
    /// The range reaches past the end of the clip, or a timestamp does.
    OutOfRange,
    /// Something else about the input was unusable.
    InvalidInput,
    /// No usable ffmpeg/ffprobe was found.
    FfmpegUnavailable,
    /// The SQLite index refused the operation.
    Store,
    /// ffmpeg or ffprobe ran and failed.
    Media,
    /// The recording engine refused the operation: no hardware encoder, a recorder that
    /// is already running, a config file that cannot describe a recording.
    Recording,
    /// A filesystem operation failed.
    Io,
}

/// A structured, serialisable command failure.
///
/// Commands never panic and never return a bare string: the frontend gets a machine
/// readable `code` and a message written for a person. Tauri's IPC boundary requires the
/// error type to be `Serialize`, which is the only constraint this type carries — it is a
/// plain value, so it is trivial to assert on in the tests below.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandError {
    pub code: ErrorCode,
    pub message: String,
}

impl CommandError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    pub fn clip_not_found(id: i64) -> Self {
        Self::new(ErrorCode::ClipNotFound, format!("no clip with id {id} is in the index"))
    }

    pub fn session_not_found(id: i64) -> Self {
        Self::new(ErrorCode::SessionNotFound, format!("no session with id {id} is in the index"))
    }

    /// `{:#}` on the wrapped error keeps the `context` chain, which is where the useful
    /// half of an `anyhow` error usually is.
    fn store(action: &str, err: anyhow::Error) -> Self {
        Self::new(ErrorCode::Store, format!("{action} failed: {err:#}"))
    }

    fn media(action: &str, err: anyhow::Error) -> Self {
        Self::new(ErrorCode::Media, format!("{action} failed: {err:#}"))
    }

    fn io(action: &str, path: &Path, err: std::io::Error) -> Self {
        Self::new(ErrorCode::Io, format!("{action} failed for {}: {err}", path.display()))
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({:?})", self.message, self.code)
    }
}

impl std::error::Error for CommandError {}

// ---------------------------------------------------------------------------------------
// Data transfer objects
// ---------------------------------------------------------------------------------------

/// One clip as the frontend sees it. Field names are snake_case, matching the Rust field
/// names and the store's columns: the TS interface in `src/lib/types.ts` mirrors this
/// exactly, and a test below pins the JSON keys so the two cannot drift silently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipDto {
    pub id: i64,
    pub path: String,
    /// Media-time position of the clip's first frame (see `localplay_store::Clip`). Not
    /// comparable across captures; the list is ordered by `created_at_ms` instead.
    pub started_at_ms: u64,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub codec: String,
    pub favourite: bool,
    /// Wall-clock instant the row was written, ms since the Unix epoch.
    pub created_at_ms: i64,
}

impl From<&Clip> for ClipDto {
    fn from(clip: &Clip) -> Self {
        Self {
            id: clip.id,
            path: clip.path.to_string_lossy().into_owned(),
            started_at_ms: clip.started_at_ms,
            duration_ms: clip.duration_ms,
            size_bytes: clip.size_bytes,
            codec: clip.codec.clone(),
            favourite: clip.favourite,
            created_at_ms: clip.created_at_ms,
        }
    }
}

/// One recording session, as the Sessions list sees it.
///
/// Mirrored by hand in `src/lib/types.ts`, and pinned by a test below that asserts the exact
/// JSON keys — the same contract the clip and storage DTOs carry.
///
/// `media_epoch_ms` is deliberately **not** here. It is the anchor the store subtracts to
/// turn an event's absolute media time into a timeline offset, and the offsets arrive
/// already computed; exposing the epoch as well would invite the frontend to do the
/// subtraction a second time, which is precisely the cross-clock defect the column was added
/// to fix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDto {
    pub id: i64,
    /// The game the watcher matched. `None` for a session nobody detected, which is the
    /// ordinary case for a manual start.
    pub game: Option<String>,
    /// `"buffer"` or `"session"` (see `localplay_store::SESSION_MODE_*`). A buffer session
    /// has no concatenated file: the row exists to group one run's clips and events.
    pub mode: String,
    /// Wall clock, ms since the Unix epoch. **Not** comparable with `offset_ms`, which is
    /// media time — two different clocks, on purpose, and the reason both are named.
    pub started_at_ms: i64,
    /// When it stopped. **`None` means it is still recording.**
    pub ended_at_ms: Option<i64>,
    /// The concatenated file, once finalised. `None` while it runs, and always for a
    /// buffer-mode session.
    pub final_path: Option<String>,
    /// Bytes the session occupies, as the store recorded them.
    pub size_bytes: i64,
    /// Exempt from both session retention rules, exactly as a favourited clip is.
    pub favourite: bool,
    /// Where the segments are, so an operator can find the footage without the database.
    pub scratch_dir: String,
    /// How long the recorded media is, in ms — the axis the review timeline is drawn against.
    ///
    /// `0` means **unknown**, not "zero length": a session recovered from a crash has no
    /// concatenated file to probe, and every row written before schema v4 predates the column.
    /// A front-end must disable its scrubber on 0 rather than draw an axis of no length and
    /// pretend the recording was empty.
    ///
    /// Deliberately not derivable from `started_at_ms`/`ended_at_ms`, which are the wall-clock
    /// window: a session whose encoder could not keep up is shorter than the clock says.
    pub duration_ms: i64,
}

impl From<&Session> for SessionDto {
    fn from(s: &Session) -> Self {
        Self {
            id: s.id,
            game: s.game.clone(),
            mode: s.mode.clone(),
            started_at_ms: s.started_at_ms,
            ended_at_ms: s.ended_at_ms,
            final_path: s.final_path.clone(),
            size_bytes: s.size_bytes,
            favourite: s.favourite,
            scratch_dir: s.scratch_dir.clone(),
            duration_ms: s.duration_ms,
        }
    }
}

/// One marker on a session's timeline, as the scrubber plots it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEventDto {
    pub id: i64,
    /// The integration's tag — `"kill"`, `"death"`, `"round_start"`, or the `"bookmark"` a
    /// hotkey clip writes. Passed through verbatim: this layer does not know the vocabulary
    /// and must not, so a tag it has never seen is a marker that exists rather than an error.
    /// The UI colours unknown tags neutrally for the same reason.
    pub kind: String,
    /// The marker's position on the **session timeline**, in ms of media time from the
    /// session's start. Computed by the store, in one SQL expression, so the scrubber, the
    /// tests and any future consumer cannot disagree about what a position means.
    pub offset_ms: i64,
    /// The integration's own detail, as JSON text, verbatim. `None` for a bookmark, which
    /// has no source to describe.
    pub payload: Option<String>,
    /// The clip this marker produced, when it produced one. `None` for a marker recorded
    /// without clipping.
    pub clip_id: Option<i64>,
}

impl From<&SessionEvent> for SessionEventDto {
    fn from(e: &SessionEvent) -> Self {
        Self {
            id: e.id,
            kind: e.kind.clone(),
            offset_ms: e.offset_ms,
            payload: e.payload.clone(),
            clip_id: e.clip_id,
        }
    }
}

/// The storage panel's data: what is on disk, what the cap is, and what the policy makes
/// of it (spec §8.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageStats {
    pub clip_count: usize,
    pub total_bytes: u64,
    pub favourite_count: usize,
    pub favourite_bytes: u64,
    pub cap_bytes: u64,
    pub max_age_days: u64,
    /// Whether the policy's verdict is that the cap can be met. `false` means the
    /// favourites alone exceed it (spec §8.1: the manager stops rather than delete a
    /// favourite to satisfy a cap it cannot meet).
    pub cap_met: bool,
    /// How far the favourited clips alone exceed the cap. Non-zero is the warning.
    pub over_cap_by_bytes: u64,
    /// How many clips the next cleanup pass would delete.
    pub planned_deletions: usize,
    /// Total bytes the library would hold after that pass.
    pub bytes_after: u64,
    pub clips_dir: String,
    /// Startup problems worth showing (a missing ffmpeg, most likely).
    pub warnings: Vec<String>,
}

/// Where a clip's cached thumbnail is, and whether it had to be made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThumbnailRef {
    pub clip_id: i64,
    pub path: String,
    pub at_ms: u64,
    /// `true` when the file already existed and ffmpeg was not run again.
    pub cached: bool,
}

/// What `delete_clip` did, in the vocabulary of spec §8.2.
///
/// The row goes first and the file second; this reports each half separately, because the
/// interesting states are the mismatches — a row deleted whose file could not be unlinked
/// is an orphan that a later sweep (or the user) has to deal with, and it must never be
/// reported as a clean delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeleteOutcome {
    pub id: i64,
    /// Whether this call's own `DELETE` removed a row. `false` means the clip was already
    /// gone from the index — in which case nothing was unlinked, because no committed row
    /// named the file (spec §8.2).
    pub row_deleted: bool,
    pub file_removed: bool,
    /// Set when the row was deleted but the file could not be unlinked.
    pub orphaned_path: Option<String>,
    /// Set when the row was deleted and the file was already absent.
    pub already_missing: bool,
    /// Bytes of file the unlink actually removed.
    pub bytes_reclaimed: u64,
    /// Cached thumbnails for this clip that were removed with it.
    pub thumbnails_removed: usize,
}

// ---------------------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------------------

/// Every clip in the index, newest first.
///
/// Ordered by `created_at_ms` (the wall clock the store stamps), **not** by
/// `clips.started_at`, which is what `Store::list_clips` orders by. `started_at` is media
/// time on the ring's own timeline: it restarts with the scratch directory, so two runs
/// can produce the same value and a comparison across them is meaningless — the store's own
/// docs say as much. `created_at` is the one comparable instant in the row, and it is also
/// what the storage policy evicts by, so the list the user sees and the order the manager
/// deletes in agree. Ties (a burst indexed in the same millisecond) are broken by id, so
/// the order is total and stable.
pub fn list_clips(deps: &Deps<'_>) -> Result<Vec<ClipDto>, CommandError> {
    let mut clips = deps.store.list_clips().map_err(|e| CommandError::store("listing clips", e))?;
    // Newest first, id breaking ties — so a burst indexed within one millisecond still has
    // a total, stable order.
    clips.sort_by_key(|clip| std::cmp::Reverse((clip.created_at_ms, clip.id)));
    Ok(clips.iter().map(ClipDto::from).collect())
}

/// The storage panel: usage, the configured cap and age limit, and the policy's verdict.
pub fn storage_stats(deps: &Deps<'_>) -> Result<StorageStats, CommandError> {
    storage_stats_at(deps, now_ms())
}

/// [`storage_stats`] against an explicit clock.
///
/// The age rule is a function of the clock, so a test that wants to see it fire would
/// otherwise have to backdate rows through SQL (the store deliberately has no API for
/// that) or sleep for a day. Taking `now_ms` as a parameter keeps the command deterministic
/// and the production wrapper a one-liner.
pub fn storage_stats_at(deps: &Deps<'_>, now_ms: i64) -> Result<StorageStats, CommandError> {
    let clips = deps.store.list_clips().map_err(|e| CommandError::store("listing clips", e))?;
    let total_bytes =
        deps.store.total_bytes().map_err(|e| CommandError::store("summing clip bytes", e))?;

    let policy = CleanupPolicy {
        max_total_bytes: deps.storage.max_total_bytes,
        max_age_days: deps.storage.max_age_days,
    };
    let plan = plan_cleanup(&clips, &policy, now_ms);

    let favourites: Vec<&Clip> = clips.iter().filter(|c| c.favourite).collect();

    Ok(StorageStats {
        clip_count: clips.len(),
        total_bytes,
        favourite_count: favourites.len(),
        favourite_bytes: favourites.iter().map(|c| c.size_bytes).sum(),
        cap_bytes: policy.max_total_bytes,
        max_age_days: policy.max_age_days,
        cap_met: plan.cap_met(),
        over_cap_by_bytes: plan.over_cap_by_bytes,
        planned_deletions: plan.deletions.len(),
        bytes_after: plan.bytes_after,
        clips_dir: deps.paths.clips_dir.to_string_lossy().into_owned(),
        warnings: deps.warnings.to_vec(),
    })
}

/// Mark a clip as a favourite, or clear it. Favourites are exempt from both storage rules
/// (spec §8.1), so this is the user's only way to protect a clip from the manager.
pub fn set_favourite(deps: &Deps<'_>, id: i64, favourite: bool) -> Result<ClipDto, CommandError> {
    let updated = deps
        .store
        .set_favourite(id, favourite)
        .map_err(|e| CommandError::store("setting the favourite flag", e))?;
    // The store reports whether a row was actually updated; `false` means the id does not
    // exist, and the UI must be told rather than shown a clip that is not there.
    if !updated {
        return Err(CommandError::clip_not_found(id));
    }
    let clip = find_clip(deps.store, id)?;
    Ok(ClipDto::from(&clip))
}

/// Cut `[start_ms, end_ms)` out of clip `id` into a **new** file next to the original, and
/// index the result.
///
/// The cut is [`localplay_media::edit::trim_lossless`] — `-c copy`, a stream copy. Nothing
/// in this path re-encodes, and nothing may: re-encoding an export is the thing principle 5
/// rules out. What a stream copy *can* cut, though, is bounded by the stream itself rather
/// than by the requested millisecond (spec §6.3 treats that as the central trade-off of the
/// whole design), so the returned clip reports the **probed** duration of the file that was
/// actually written rather than echoing the span that was asked for. The UI shows the two
/// side by side instead of pretending they always agree.
pub fn trim_clip(
    deps: &Deps<'_>,
    id: i64,
    start_ms: u64,
    end_ms: u64,
) -> Result<ClipDto, CommandError> {
    let clip = find_clip(deps.store, id)?;
    validate_trim_range(&clip, start_ms, end_ms)?;

    let bins = deps.binaries()?;
    if !clip.path.is_file() {
        return Err(CommandError::new(
            ErrorCode::Io,
            format!(
                "clip #{id} is indexed at {} but that file is not on disk, so there is \
                 nothing to trim",
                clip.path.display()
            ),
        ));
    }

    let dst = free_output_path(trim_output_path(&clip.path, start_ms, end_ms))?;
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| CommandError::io("creating the clips directory", dir, e))?;
    }

    trim_lossless(bins, &clip.path, &dst, start_ms, end_ms)
        .map_err(|e| CommandError::media("trimming the clip", e))?;

    // The file exists from here on. Everything below can fail, and if it does the file
    // stays: it is what the user asked for, and deleting it to report a bookkeeping error
    // would destroy the only copy of their edit.
    index_new_clip(
        deps,
        &format!("clip #{}", clip.id),
        clip.started_at_ms,
        &clip.codec,
        &dst,
        start_ms,
        end_ms,
    )
}

/// Describe a newly written file and add it to the index.
///
/// The parent is passed as what it *is* — a label for the log, the media position the new
/// clip should be placed at, and a codec to name when the new file turns out to have no video
/// stream — rather than as a `&Clip`. A session's extracted clip has no parent `Clip`: a
/// session is not a clip, and inventing one to satisfy a parameter would put a fake row's id
/// in a log line and a fake `started_at` on a real clip.
fn index_new_clip(
    deps: &Deps<'_>,
    parent: &str,
    parent_started_at_ms: u64,
    parent_codec: &str,
    dst: &Path,
    start_ms: u64,
    end_ms: u64,
) -> Result<ClipDto, CommandError> {
    let bins = deps.binaries()?;
    let info = MediaInfo::probe(bins, dst).map_err(|e| {
        CommandError::media(
            &format!(
                "reading back the trimmed clip {} (the file was written and is on disk, \
                 but it could not be indexed)",
                dst.display()
            ),
            e,
        )
    })?;

    // Sized from the file rather than from ffprobe's `format.size`, because the number the
    // storage policy caps is a number of bytes on disk.
    let size_bytes = std::fs::metadata(dst)
        .map_err(|e| CommandError::io("sizing the trimmed clip", dst, e))?
        .len();

    let codec = info
        .video
        .as_ref()
        .map(|v| v.codec.clone())
        // A file with no video stream is not a clip this app can show; falling back to the
        // parent's codec would name a codec the file may not contain, so say what is known.
        .unwrap_or_else(|| parent_codec.to_string());

    // `started_at` is the trimmed clip's position on its **parent's** media timeline. The
    // file's own timeline starts at zero, and nothing compares `started_at` across clips
    // or runs, so this is the only reading of the column that carries information. It is an
    // estimate rather than a measurement: a stream copy cannot place a cut at an arbitrary
    // point in the stream, so the first frame of the file may sit slightly before the
    // `start_ms` that was asked for (spec §6.3).
    let new = NewClip {
        path: dst.to_path_buf(),
        started_at_ms: parent_started_at_ms.saturating_add(start_ms),
        duration_ms: info.duration_ms,
        size_bytes,
        codec,
    };

    let new_id = deps.store.insert_clip(&new).map_err(|e| {
        CommandError::new(
            ErrorCode::Store,
            // The file is on disk with no row naming it, so the storage policy will never
            // collect it. Saying so is the difference between a user who can act and one
            // with an invisible file filling their disk.
            format!(
                "the trimmed clip was written to {} but could not be added to the index, \
                 so it will not appear in the library and the storage policy cannot manage \
                 it: {e:#}",
                dst.display()
            ),
        )
    })?;

    tracing::info!(
        "{parent} [{start_ms}ms, {end_ms}ms) was written as clip #{new_id}: {size_bytes} \
         bytes, probed duration {}ms, {}",
        info.duration_ms,
        dst.display()
    );

    // Re-read the row so the DTO carries the store's own `created_at` stamp.
    Ok(ClipDto::from(&find_clip(deps.store, new_id)?))
}

/// A JPEG thumbnail of clip `id` at `at_ms`, made once and reused afterwards.
///
/// The file lands in the thumbnails cache directory under a name derived from the clip and
/// the timestamp, so a second request for the same frame is a `stat` rather than another
/// ffmpeg process.
///
/// A timestamp at or past `duration_ms` is rejected here rather than handed to ffmpeg. A
/// timestamp *inside* the clip can still yield no frame — the last frame of a 2000ms stream
/// does not start at 1999ms — and that surfaces as an [`ErrorCode::Media`] error naming the
/// clip, the timestamp and the duration, because "ffmpeg had nothing to read" is otherwise
/// indistinguishable from "the file is corrupt". The UI asks for a frame from the first
/// second of a clip for exactly that reason (see `thumbnailAtMs` in `src/lib/clips.ts`).
pub fn thumbnail(deps: &Deps<'_>, id: i64, at_ms: u64) -> Result<ThumbnailRef, CommandError> {
    let clip = find_clip(deps.store, id)?;
    if clip.duration_ms == 0 {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "clip #{id} is indexed with a duration of 0ms, so there is no frame to \
                 read a thumbnail from"
            ),
        ));
    }
    if at_ms >= clip.duration_ms {
        return Err(CommandError::new(
            ErrorCode::OutOfRange,
            format!(
                "a thumbnail was requested at {at_ms}ms but clip #{id} is only {}ms long",
                clip.duration_ms
            ),
        ));
    }

    let bins = deps.binaries()?;
    let dst = deps.paths.thumbnails_dir.join(thumbnail_file_name(id, at_ms));
    if dst.is_file() {
        return Ok(ThumbnailRef {
            clip_id: id,
            path: dst.to_string_lossy().into_owned(),
            at_ms,
            cached: true,
        });
    }
    std::fs::create_dir_all(&deps.paths.thumbnails_dir)
        .map_err(|e| CommandError::io("creating the thumbnails directory", &deps.paths.thumbnails_dir, e))?;

    ffmpeg_thumbnail(bins, &clip.path, at_ms, &dst).map_err(|e| {
        CommandError::media(
            &format!(
                "reading a frame from clip #{id} at {at_ms}ms, which is inside the clip's \
                 {}ms but may be past the start of its last frame",
                clip.duration_ms
            ),
            e,
        )
    })?;

    Ok(ThumbnailRef {
        clip_id: id,
        path: dst.to_string_lossy().into_owned(),
        at_ms,
        cached: false,
    })
}

/// Delete a clip: the row first, then the file (spec §8.2).
///
/// The order is the safety property, not a detail: deleting the row first means the index
/// never names a file that is gone, which is the failure a user cannot recover from,
/// because the file is the only copy. The reverse failure — a file whose row is gone — is
/// recoverable and is *reported* here in [`DeleteOutcome::orphaned_path`] rather than
/// silently swallowed.
///
/// An id with no row is not an error: it is reported as `row_deleted: false`, and nothing
/// is unlinked, because no committed row delete named the file. A UI acting on a list it
/// fetched a moment ago can race a cleanup pass, and telling it "already gone" lets it
/// refresh truthfully.
pub fn delete_clip(deps: &Deps<'_>, id: i64) -> Result<DeleteOutcome, CommandError> {
    // Committed before the next line runs: the store's own contract, and the reason the
    // path handed back here is a path a committed row delete removed.
    let path = deps
        .store
        .delete_clip_returning_path(id)
        .map_err(|e| CommandError::store("deleting the clip's row", e))?;

    let Some(path) = path else {
        return Ok(DeleteOutcome {
            id,
            row_deleted: false,
            file_removed: false,
            orphaned_path: None,
            already_missing: false,
            bytes_reclaimed: 0,
            thumbnails_removed: 0,
        });
    };

    let path = PathBuf::from(path);
    let mut outcome = DeleteOutcome {
        id,
        row_deleted: true,
        file_removed: false,
        orphaned_path: None,
        already_missing: false,
        bytes_reclaimed: 0,
        // Best effort, and reported rather than fatal: a stale JPEG in the cache is a
        // wasted kilobyte, not a reason to fail a delete whose row is already committed.
        thumbnails_removed: remove_cached_thumbnails(&deps.paths.thumbnails_dir, id),
    };

    // Sized before the unlink so the reported figure is what was actually freed.
    let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            outcome.file_removed = true;
            outcome.bytes_reclaimed = size_bytes;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                "clip #{id} was deleted from the index but its file was already gone: {}",
                path.display()
            );
            outcome.already_missing = true;
        }
        Err(err) => {
            tracing::warn!(
                "clip #{id} was deleted from the index but {} could not be unlinked \
                 ({err}); it is orphaned on disk",
                path.display()
            );
            outcome.orphaned_path = Some(path.to_string_lossy().into_owned());
        }
    }

    Ok(outcome)
}

// ---------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------

fn find_clip(store: &Store, id: i64) -> Result<Clip, CommandError> {
    store
        .list_clips()
        .map_err(|e| CommandError::store("reading the clip index", e))?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| CommandError::clip_not_found(id))
}

/// Reject a trim range before ffmpeg is spawned.
///
/// The first branch is the one the UI must never hit, because it clamps before calling:
/// `trim_lossless` would bail on it too, but with a message that names no clip. The second
/// exists so that a range reaching past the end of the clip is an explicit, actionable
/// error rather than an ffmpeg failure to decode.
fn validate_trim_range(clip: &Clip, start_ms: u64, end_ms: u64) -> Result<(), CommandError> {
    validate_range("clip", clip.id, clip.duration_ms, start_ms, end_ms)
}

/// The same rules for anything a range can be cut out of — a clip, or a session recording.
///
/// `subject` names which, so the message a user reads says "clip #7" or "session #3" instead
/// of making them work out which one they asked for.
fn validate_range(
    subject: &str,
    id: i64,
    duration_ms: u64,
    start_ms: u64,
    end_ms: u64,
) -> Result<(), CommandError> {
    if end_ms <= start_ms {
        return Err(CommandError::new(
            ErrorCode::InvalidRange,
            format!(
                "the range for {subject} #{id} is empty or inverted: start={start_ms}ms, \
                 end={end_ms}ms. A range with no length cannot be cut."
            ),
        ));
    }
    if duration_ms == 0 {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!("{subject} #{id} has a duration of 0ms, so it cannot be cut from"),
        ));
    }
    if end_ms > duration_ms {
        return Err(CommandError::new(
            ErrorCode::OutOfRange,
            format!(
                "the range for {subject} #{id} ends at {end_ms}ms but it is only {duration_ms}ms \
                 long"
            ),
        ));
    }
    Ok(())
}

/// Where a trim of `[start_ms, end_ms)` of `src` is written: beside the original, with the
/// range in the name so the file explains itself in a directory listing.
///
/// The extension is preserved. Clips are `mp4` in this application, and `trim_lossless`
/// muxes with `-movflags +faststart`, which only an ISO-BMFF muxer accepts — so the
/// extension is carried over rather than assumed.
pub fn trim_output_path(src: &Path, start_ms: u64, end_ms: u64) -> PathBuf {
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clip".to_string());
    let ext = src
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mp4".to_string());
    src.with_file_name(format!("{stem}.trim-{start_ms}-{end_ms}.{ext}"))
}

/// The first name derived from `candidate` that no file on disk already holds.
///
/// Bounded, and a bound that is reached is an error rather than an overwrite: silently
/// clobbering a previous trim would destroy a file the index may name, which is exactly
/// the class of loss spec §8.2 is written to prevent.
fn free_output_path(candidate: PathBuf) -> Result<PathBuf, CommandError> {
    if !candidate.exists() {
        return Ok(candidate);
    }
    let stem = candidate
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clip".to_string());
    let ext = candidate
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mp4".to_string());

    for n in 2..1000u32 {
        let next = candidate.with_file_name(format!("{stem}-{n}.{ext}"));
        if !next.exists() {
            return Ok(next);
        }
    }
    Err(CommandError::new(
        ErrorCode::Io,
        format!(
            "no free name is available for the trimmed clip: {} and 997 numbered copies \
             of it already exist. Move or delete some of them and try again.",
            candidate.display()
        ),
    ))
}

/// The cache file name for one clip's thumbnail at one timestamp.
pub fn thumbnail_file_name(clip_id: i64, at_ms: u64) -> String {
    format!("clip-{clip_id}-{at_ms}.jpg")
}

/// Remove every cached thumbnail belonging to `clip_id`. Returns how many went.
///
/// The prefix match is exact about the id: `clip-1-` must not match `clip-12-0.jpg`, which
/// is why the trailing dash is part of it.
fn remove_cached_thumbnails(dir: &Path, clip_id: i64) -> usize {
    let prefix = format!("clip-{clip_id}-");
    let Ok(entries) = std::fs::read_dir(dir) else {
        // No cache directory is the normal case for a library whose clips were never
        // thumbnailed; it is not a failure.
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(".jpg") {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(err) => tracing::warn!(
                "could not remove the cached thumbnail {}: {err}",
                entry.path().display()
            ),
        }
    }
    removed
}

/// Milliseconds since the Unix epoch, saturating to 0 for a clock before it.
///
/// The store stamps `created_at` from its own copy of this. Nothing here is a measurement
/// of elapsed time; it only has to be the same wall clock the store is on.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------------------
//
// Phase 5's session recorder writes every second it was told to into one scratch directory,
// then concatenates it at the end. This is the window onto that. Two things are worth
// naming, because the obvious implementations get them wrong:
//
// * `offset_ms` comes from the **store**, computed from the session's media epoch. Nothing
//   here recomputes it against the wall clock. That recomputation *was* the defect — a
//   timeline measured across two clocks — and it stays fixed only for as long as the
//   subtraction lives in one place.
// * `extract_clip` refuses a session that is still recording. Its scratch directory is the
//   only copy of the footage, so a cut taken while the encoder is still writing into it
//   would be a cut from a moving target.

/// One session row, or a `session_not_found` naming the id.
fn load_session(deps: &Deps<'_>, id: i64) -> Result<Session, CommandError> {
    deps.store
        .get_session(id)
        .map_err(|e| CommandError::store("reading the session", e))?
        .ok_or_else(|| CommandError::session_not_found(id))
}

/// Every session the index knows, newest first.
///
/// The order is the store's own (`ORDER BY started_at DESC, id DESC`) rather than re-sorted
/// here: one definition of "newest", and it is the one the retention rules order by too.
pub fn list_sessions(deps: &Deps<'_>) -> Result<Vec<SessionDto>, CommandError> {
    let sessions = deps
        .store
        .list_sessions()
        .map_err(|e| CommandError::store("listing the sessions", e))?;
    Ok(sessions.iter().map(SessionDto::from).collect())
}

/// One session's row, for the detail panel.
pub fn session_detail(deps: &Deps<'_>, session_id: i64) -> Result<SessionDto, CommandError> {
    Ok(SessionDto::from(&load_session(deps, session_id)?))
}

/// A session's timeline, in media-time order.
///
/// An empty vector is a real answer: a session nobody tagged has no markers. A *missing*
/// session is not — it is a [`ErrorCode::SessionNotFound`], which is why the row is checked
/// first rather than letting an unknown id look like a session with an empty timeline.
pub fn session_events(
    deps: &Deps<'_>,
    session_id: i64,
) -> Result<Vec<SessionEventDto>, CommandError> {
    load_session(deps, session_id)?;
    let events = deps
        .store
        .events_for_session(session_id)
        .map_err(|e| CommandError::store("reading the session timeline", e))?;
    Ok(events.iter().map(SessionEventDto::from).collect())
}

/// Mark a session as a favourite, or clear it — the user's only way to protect one from the
/// session retention rules, exactly as it is for a clip.
pub fn set_session_favourite(
    deps: &Deps<'_>,
    session_id: i64,
    favourite: bool,
) -> Result<SessionDto, CommandError> {
    // Checked first so a bogus id is a `session_not_found` rather than the store's own
    // "nothing was favourited", which carries no code the frontend can switch on.
    load_session(deps, session_id)?;
    deps.store
        .set_session_favourite(session_id, favourite)
        .map_err(|e| CommandError::store("setting the session's favourite flag", e))?;
    session_detail(deps, session_id)
}

/// What deleting a session actually did.
///
/// The row goes first and the bytes after, for the same reason [`DeleteOutcome`] exists: the
/// index must never name files that are gone. What differs with a session is that the bytes
/// are a *directory* of segments plus, usually, one concatenated file — so "did it work" is
/// two answers, and both are reported rather than collapsed into one boolean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteSessionOutcome {
    pub id: i64,
    /// False when nothing named that id. A UI acting on a list it fetched a moment ago can
    /// race a retention pass, and telling it "already gone" lets it refresh truthfully —
    /// the same contract as [`DeleteOutcome::row_deleted`].
    pub row_deleted: bool,
    /// The scratch directory of segments is gone.
    pub scratch_removed: bool,
    /// The concatenated session file is gone.
    pub final_file_removed: bool,
    /// Bytes freed, measured from what was on disk immediately before removal.
    pub bytes_reclaimed: u64,
    /// Paths the committed row delete named that could not be removed afterwards. Reported
    /// rather than swallowed: an orphan is recoverable, a silent one is not.
    pub orphaned: Vec<String>,
}

/// Delete a session: its row, then its scratch directory and its concatenated file.
///
/// **A session that is still recording is refused** with
/// [`ErrorCode::SessionStillRecording`]. The store refuses it too, and that is the check that
/// guarantees it; this one exists so the frontend can say *why* without parsing a message.
pub fn delete_session(
    deps: &Deps<'_>,
    session_id: i64,
) -> Result<DeleteSessionOutcome, CommandError> {
    // The row is read first so a *running* session is refused with a code the frontend can
    // switch on. A missing row is **not** an error, it is `row_deleted: false` — the same
    // answer `delete_clip` gives, so a UI acting on a list it fetched a moment ago can race a
    // retention pass and still refresh truthfully.
    let Some(session) = deps
        .store
        .get_session(session_id)
        .map_err(|e| CommandError::store("reading the session", e))?
    else {
        return Ok(DeleteSessionOutcome {
            id: session_id,
            row_deleted: false,
            scratch_removed: false,
            final_file_removed: false,
            bytes_reclaimed: 0,
            orphaned: Vec::new(),
        });
    };
    if session.ended_at_ms.is_none() {
        return Err(CommandError::new(
            ErrorCode::SessionStillRecording,
            format!(
                "session #{session_id} is still recording: its scratch directory is the \
                 recording, and no ordering makes deleting it safe. Stop the recording first."
            ),
        ));
    }

    // Committed before anything is unlinked, and the paths are the ones a committed row
    // delete named — the store's own contract.
    let paths = deps
        .store
        .delete_session_returning_paths(session_id)
        .map_err(|e| CommandError::store("deleting the session's row", e))?;
    let Some(paths) = paths else {
        // Raced away between the read above and this delete. Nothing is unlinked, because no
        // committed row delete named anything.
        return Ok(DeleteSessionOutcome {
            id: session_id,
            row_deleted: false,
            scratch_removed: false,
            final_file_removed: false,
            bytes_reclaimed: 0,
            orphaned: Vec::new(),
        });
    };

    let mut outcome = DeleteSessionOutcome {
        id: session_id,
        row_deleted: true,
        scratch_removed: false,
        final_file_removed: false,
        bytes_reclaimed: 0,
        orphaned: Vec::new(),
    };

    // The recorded file is removed **before** the scratch directory. They are separate
    // locations in the normal layout — the segments go in the session's scratch directory and
    // the concatenated file in the sessions output directory — but nothing enforces that: the
    // store takes both as caller-supplied strings, so a layout that did place the file among
    // the segments would otherwise have its removal reported as a failure, because
    // `remove_dir_all` had already taken it. File first, and the nested case is correct too.
    //
    // A buffer-mode session has no file, which is not a failure.
    if let Some(final_path) = paths.final_path {
        let final_path = PathBuf::from(final_path);
        let size = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
        match std::fs::remove_file(&final_path) {
            Ok(()) => {
                outcome.final_file_removed = true;
                outcome.bytes_reclaimed += size;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::error!(
                    "session #{session_id} was deleted from the index but its recorded file \
                     could not be removed: {} ({err})",
                    final_path.display()
                );
                outcome.orphaned.push(final_path.to_string_lossy().into_owned());
            }
        }
    }

    // The store hands these back as strings (they come out of the database), so they are
    // turned into paths once, here, rather than at each use.
    let scratch_dir = PathBuf::from(&paths.scratch_dir);
    let scratch = dir_size(&scratch_dir);
    match std::fs::remove_dir_all(&scratch_dir) {
        Ok(()) => {
            outcome.scratch_removed = true;
            outcome.bytes_reclaimed += scratch;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                "session #{session_id} was deleted from the index but its scratch directory \
                 was already gone: {}",
                scratch_dir.display()
            );
        }
        Err(err) => {
            tracing::error!(
                "session #{session_id} was deleted from the index but its scratch directory \
                 could not be removed: {} ({err})",
                scratch_dir.display()
            );
            outcome.orphaned.push(scratch_dir.to_string_lossy().into_owned());
        }
    }

    Ok(outcome)
}

/// Cut `[start_ms, end_ms)` out of a **finished** session into a new clip, and index it.
///
/// The same lossless path [`trim_clip`] uses — read that for what a stream copy can and
/// cannot cut — with one extra refusal: a session that is still recording cannot be cut
/// from, because the file the range was measured against is not the file that would be read.
///
/// A buffer-mode session has no concatenated file at all (it keeps a rolling ring), so the
/// range has nothing to be measured against; that is refused with the reason, and pressing
/// the clip hotkey is what a running buffer is for.
pub fn extract_clip(
    deps: &Deps<'_>,
    session_id: i64,
    start_ms: u64,
    end_ms: u64,
) -> Result<ClipDto, CommandError> {
    let session = load_session(deps, session_id)?;
    if session.ended_at_ms.is_none() {
        return Err(CommandError::new(
            ErrorCode::SessionStillRecording,
            format!(
                "session #{session_id} is still recording, so there is no finished file to cut \
                 from. Stop the recording, then extract the clip."
            ),
        ));
    }
    let Some(final_path) = session.final_path.as_deref() else {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "session #{session_id} has no concatenated file: it was recorded in buffer \
                 mode, which keeps a rolling ring rather than one session file. Use the clip \
                 hotkey while it is running."
            ),
        ));
    };
    let src = PathBuf::from(final_path);
    if !src.is_file() {
        return Err(CommandError::new(
            ErrorCode::Io,
            format!(
                "session #{session_id} names {} as its recorded file, but that is not on disk, \
                 so there is nothing to cut from",
                src.display()
            ),
        ));
    }

    let bins = deps.binaries()?;
    // The range is validated against the file's own probed duration, not against a stored
    // number: the only duration that matters is the one in the file about to be cut.
    let info = MediaInfo::probe(bins, &src)
        .map_err(|e| CommandError::media("reading the session file's duration", e))?;
    validate_range("session", session_id, info.duration_ms, start_ms, end_ms)?;
    let Some(video) = info.video.as_ref() else {
        return Err(CommandError::new(
            ErrorCode::Media,
            format!(
                "{} has no video stream, so there is no picture to cut a clip from",
                src.display()
            ),
        ));
    };

    let dst = free_output_path(trim_output_path(&src, start_ms, end_ms))?;
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| CommandError::io("creating the clips directory", dir, e))?;
    }

    trim_lossless(bins, &src, &dst, start_ms, end_ms)
        .map_err(|e| CommandError::media("extracting the clip from the session", e))?;

    // The new row's `started_at` is the clip's position on the **session's** media timeline,
    // which is what the session's markers are measured on — so a clip extracted at a marker
    // sits at that marker. The file's own timeline starts at zero, and nothing compares
    // `started_at` across files, so this is the only reading that carries information. An
    // estimate, not a measurement: a stream copy cannot cut at an arbitrary point.
    let parent_started_at_ms = session.media_epoch_ms.max(0) as u64;
    index_new_clip(
        deps,
        &format!("session #{session_id}"),
        parent_started_at_ms,
        &video.codec,
        &dst,
        start_ms,
        end_ms,
    )
}

/// Total bytes of every file under `dir`, or 0 when it cannot be walked.
///
/// Used only to report what a deletion freed, so a directory that cannot be read is 0 rather
/// than an error: the deletion itself reports its own failure separately, and refusing to
/// delete because the *measurement* failed would be backwards.
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            let path = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => dir_size(&path),
                _ => std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            }
        })
        .sum()
}

// ---------------------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------------------
//
// The engine itself is `localplay-recorder` — the same one the CLI drives. What is here is
// the shell's side of it: one slot that holds the recording this window started, and the
// plain functions the `#[tauri::command]` wrappers delegate to.
//
// Two properties are worth naming, because the obvious implementations get them wrong:
//
// * `recording_status` never blocks the recording loop. The engine publishes its counters
//   through atomics (`Recorder::status`), so a poll every 500ms costs a handful of loads
//   and cannot disturb a capture that is trying to keep up with 60fps.
// * A clip trigger *does* block — `clip_now` waits for the post-roll to be written, which
//   is `post_seconds` of media plus a margin. The slot lock is therefore released before
//   the wait: the caller clones a handle out of the slot and blocks on that, so the
//   window's status poll keeps answering while a clip is being saved.

/// The live recording status, as the frontend sees it.
///
/// Mirrored by hand in `src/lib/types.ts`, and pinned by a test below that asserts the
/// exact JSON keys — the same contract the clip and storage DTOs carry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingStatusDto {
    /// Whether a recording is running. `false` also means "never started".
    pub running: bool,
    /// Video frames submitted to the encoder this session.
    pub frames: u64,
    /// Completed segments in the scratch ring.
    pub segments: u64,
    /// Bytes the ring holds on disk.
    pub bytes: u64,
    /// Media time on disk, in ms: how much footage a clip can be cut from.
    pub span_ms: u64,
    /// Encoder frames dropped because its queue was full (the machine cannot keep up).
    pub dropped: u64,
    /// The same for audio blocks.
    pub dropped_audio: u64,
    /// Frames the capture backend offered and the pacer skipped without reading back.
    pub skipped: u64,
    /// The achieved frame rate over the last second; 0.0 before one has been measured.
    pub fps: f64,
    /// What `encode.fps` asked for, to show the two side by side.
    pub configured_fps: u32,
    /// The rate the pipeline is actually running at — the pacer's rate and the encoder
    /// child's `-framerate`, one number, chosen at startup from what the throughput probe
    /// measured. Equal to `configured_fps` unless this machine could not hold the
    /// configured rate at the captured resolution, in which case the engine has already
    /// logged why. It is the rate the media timeline is recorded at, so it is what an
    /// achieved-rate readout belongs next to.
    pub effective_fps: u32,
    /// Wall clock minus media time, in ms. Positive means the media timeline is behind
    /// real time, which is why a trigger is taken from `span_ms` rather than the clock.
    pub drift_ms: i64,
    /// Clips this session has written.
    pub clips: u64,
    /// Why the engine stopped, when it stopped for a failure rather than a `stop`.
    pub error: Option<String>,
}

impl From<RecorderStatus> for RecordingStatusDto {
    fn from(status: RecorderStatus) -> Self {
        Self {
            running: status.running,
            frames: status.frames,
            segments: status.segments,
            bytes: status.bytes,
            span_ms: status.span_ms,
            dropped: status.dropped,
            dropped_audio: status.dropped_audio,
            skipped: status.skipped,
            fps: status.fps,
            configured_fps: status.configured_fps,
            effective_fps: status.effective_fps,
            drift_ms: status.drift_ms,
            clips: status.clips,
            error: status.error,
        }
    }
}

/// What a clip trigger produced.
///
/// Not a [`ClipDto`]: a clip that was written but could not be indexed has no row, and
/// reporting it as a clip with a made-up id would hide exactly the failure the index write
/// has to be honest about (spec §8.2). `id` is the row when there is one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedClipDto {
    /// The `clips` row, or `null` when the file was written but not indexed — in which
    /// case the UI must say so, because the clip will not appear in the list.
    pub id: Option<i64>,
    pub path: String,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub codec: String,
    /// The clip's first frame on the engine's media timeline.
    pub started_at_ms: u64,
}

/// The one recording this shell can have running, plus what a start is built from.
///
/// The engine is an `Arc<Recorder>` rather than a `Recorder` so a command can take a
/// handle and drop the slot lock before doing slow work (see the note above about
/// `clip_now`). `Mutex` because the commands run on more than one thread.
pub struct RecorderHost {
    /// The directory `scratch/`, `clips/` and `localplay.db` resolve against, and where
    /// `config.toml` is read from when a recording starts.
    app_data_dir: PathBuf,
    /// ffmpeg, or `None` when it could not be found at startup — in which case recording
    /// is impossible and every start says so rather than failing somewhere deeper.
    bins: Option<FfmpegBinaries>,
    current: Mutex<Option<Arc<Recorder>>>,
}

impl RecorderHost {
    pub fn new(app_data_dir: PathBuf, bins: Option<FfmpegBinaries>) -> Self {
        Self { app_data_dir, bins, current: Mutex::new(None) }
    }

    /// The `config.toml` a recording would be started from.
    pub fn config_path(&self) -> PathBuf {
        self.app_data_dir.join("config.toml")
    }

    /// The slot, or the error a poisoned slot deserves.
    ///
    /// Poisoning means a command panicked while holding the slot. Nothing in this module
    /// panics, so if it happens it is a bug, and the honest answer is to refuse rather
    /// than to pretend no recording is running.
    fn slot(&self) -> Result<MutexGuard<'_, Option<Arc<Recorder>>>, CommandError> {
        self.current.lock().map_err(|_| {
            CommandError::new(
                ErrorCode::Recording,
                "the recording state is unusable: an earlier command panicked while \
                 holding it",
            )
        })
    }

    /// A handle on the running recording, with the slot lock already released.
    fn handle(&self) -> Result<Option<Arc<Recorder>>, CommandError> {
        Ok(self.slot()?.as_ref().map(Arc::clone))
    }

    /// What the recorder is doing right now — never blocks the recording loop, and answers
    /// "not running" before anything has been started.
    pub fn status(&self) -> Result<RecordingStatusDto, CommandError> {
        Ok(match self.handle()? {
            Some(recorder) => RecordingStatusDto::from(recorder.status()),
            None => RecordingStatusDto::from(RecorderStatus::stopped()),
        })
    }

    /// Start recording, and return the first status.
    ///
    /// Everything the engine decides at startup (the encoder smoke test, the capture
    /// backend, the ring, ffmpeg) happens here, on the caller's thread, so a machine that
    /// cannot record says so now instead of failing inside a background thread.
    pub fn start(&self, settings: &RecordingConfig) -> Result<RecordingStatusDto, CommandError> {
        self.start_with(settings, Sources::Platform, false)
    }

    /// Start from the `config.toml` this host reads, exactly as the window's Start button
    /// does.
    ///
    /// The tray's "Start recording" item and the hotkey's own recording both go through here
    /// rather than loading the file themselves, so there is one place where "what a start
    /// means" is decided — including the fact that the file is re-read at start time and not
    /// at window-open time (see `config.rs`).
    pub fn start_from_config(&self) -> Result<RecordingStatusDto, CommandError> {
        let settings = RecordingConfig::load(&self.config_path())?;
        self.start(&settings)
    }

    /// The body of a start. `sources` and `dev_software_encoder` are the engine's own
    /// choices, passed through unchanged: shipping code takes [`Self::start`], which is
    /// exactly [`Sources::Platform`] with the software encoder off.
    fn start_with(
        &self,
        settings: &RecordingConfig,
        sources: Sources,
        dev_software_encoder: bool,
    ) -> Result<RecordingStatusDto, CommandError> {
        let bins = self.bins.as_ref().ok_or_else(|| {
            CommandError::new(
                ErrorCode::FfmpegUnavailable,
                "ffmpeg was not found, so a recording cannot be started. Install ffmpeg on \
                 PATH or place the sidecar binaries next to the application.",
            )
        })?;

        // The lock is held across the start so two concurrent starts cannot both win; a
        // start is a startup handshake (a one-frame smoke test at most), not a wait.
        let mut slot = self.slot()?;
        if slot.as_ref().is_some_and(|recorder| recorder.is_running()) {
            return Err(CommandError::new(
                ErrorCode::Recording,
                "a recording is already running: stop it before starting another",
            ));
        }

        let cfg = RecorderConfig {
            bin: bins.clone(),
            app_data_dir: self.app_data_dir.clone(),
            buffer: settings.buffer.clone(),
            encode: settings.encode.clone(),
            storage: settings.storage.clone(),
            // WGC + WASAPI on Windows, the synthetic stubs everywhere else. This shell has
            // no switch for it and must not: the engine refuses to substitute a stub on
            // Windows, so there is no path here that captures nothing while looking like a
            // recording.
            sources,
            dev_software_encoder,
        };

        let recorder = Recorder::start(cfg).map_err(|err| {
            CommandError::new(ErrorCode::Recording, format!("could not start recording: {err:#}"))
        })?;
        let status = RecordingStatusDto::from(recorder.status());
        *slot = Some(Arc::new(recorder));
        Ok(status)
    }

    /// **TEST-ONLY.** Start with the engine's synthetic sources and its software encoder.
    ///
    /// Why this exists: the tray and the hotkey both end at [`Self::clip_now`], and the only
    /// convincing test of that path is a *real* recording — a real encoder child process, a
    /// real ring on disk, a real splice and a real index row. [`Sources::Platform`] is that
    /// path on Windows but is a live Windows Graphics Capture session there, which no test
    /// may open; [`Sources::Stub`] is the same engine over synthetic frames, and it is
    /// unreachable from a configuration file by construction (`localplay_recorder::Sources`
    /// documents that). Gated on `cfg(test)`, so a shipped binary has no such entry point at
    /// all — the CLI's equivalent is its `--dev-software-encoder` feature.
    #[cfg(test)]
    pub fn start_for_test_with_stub_sources(
        &self,
        settings: &RecordingConfig,
    ) -> Result<RecordingStatusDto, CommandError> {
        let stub = StubConfig { width: 1280, height: 720, fps: settings.encode.fps };
        self.start_with(settings, Sources::Stub(stub), true)
    }

    /// Stop the recording, flush the encoder and return the final status.
    ///
    /// Idempotent: with nothing running it reports the idle status rather than failing, so
    /// a stop from a window whose recording already died is not an error the user has to
    /// make sense of.
    pub fn stop(&self) -> Result<RecordingStatusDto, CommandError> {
        // Held across the stop: the join returns within one frame poll, and holding it is
        // what makes "stop then start" a sequence rather than a race between a capture
        // session that is closing and one that is opening.
        let taken = self.slot()?.take();
        let Some(recorder) = taken else {
            return Ok(RecordingStatusDto::from(RecorderStatus::stopped()));
        };
        let outcome = recorder.stop().map_err(|err| {
            CommandError::new(ErrorCode::Recording, format!("stopping the recorder failed: {err:#}"))
        });
        let status = RecordingStatusDto::from(recorder.status());
        // The status is reported either way — a recorder that failed while stopping still
        // has counters worth showing — but a failed stop is still a failure.
        outcome.map(|()| status)
    }

    /// Take a clip now.
    ///
    /// Blocks for the post-roll (seconds), which is why the caller must be off the UI
    /// thread: the whole point of this command is that the footage is on disk before it
    /// returns.
    pub fn clip_now(&self) -> Result<RecordedClipDto, CommandError> {
        let recorder = self.handle()?.ok_or_else(|| {
            CommandError::new(
                ErrorCode::Recording,
                "nothing is recording, so there is no buffer to take a clip from",
            )
        })?;
        let clip = recorder.clip_now().map_err(|err| {
            CommandError::new(ErrorCode::Recording, format!("taking a clip failed: {err:#}"))
        })?;
        Ok(RecordedClipDto {
            id: clip.id,
            path: clip.metadata.path.to_string_lossy().into_owned(),
            duration_ms: clip.metadata.duration_ms,
            size_bytes: clip.metadata.size_bytes,
            codec: clip.metadata.encoder,
            started_at_ms: clip.started_at_ms,
        })
    }
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

/// The command bodies, driven directly against a real SQLite file and real ffmpeg output.
///
/// No Tauri runtime, no window, no state: [`Deps`] is a struct of references precisely so
/// that this module can build one. The two things the suite deliberately does *not* fake
/// are the store (a real `localplay.db` in a temp directory, migrated) and the media (real
/// `mpeg4` files written by ffmpeg, and the real sidecar binaries the app will use) —
/// because those are where the mistakes live. `mpeg4` rather than H.264 keeps the tests
/// runnable on an LGPL ffmpeg build and needs no hardware encoder.
#[cfg(test)]
mod tests {
    use super::*;
    use localplay_media::binaries::run_with_timeout;
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// A cap nothing reaches and an age nothing exceeds: the policy is a no-op, so a test
    /// that is not about the policy cannot trip over it.
    const GENEROUS: StorageConfigView =
        StorageConfigView { max_total_bytes: u64::MAX, max_age_days: 3_650 };

    struct Fixture {
        /// Held so the temporary directory outlives the store and the clips in it.
        _dir: tempfile::TempDir,
        store: Store,
        paths: AppPaths,
        storage: StorageConfigView,
        bins: FfmpegBinaries,
        warnings: Vec<String>,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_storage(GENEROUS)
        }

        fn with_storage(storage: StorageConfigView) -> Self {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let bins = FfmpegBinaries::discover(None)
                .expect("ffmpeg and ffprobe must be on PATH to run these tests");
            let paths = AppPaths::resolve(dir.path(), "");
            std::fs::create_dir_all(&paths.clips_dir).unwrap();
            let store = Store::open(&paths.db_path).unwrap();
            store.migrate().unwrap();
            Self { _dir: dir, store, paths, storage, bins, warnings: Vec::new() }
        }

        fn deps(&self) -> Deps<'_> {
            Deps {
                store: &self.store,
                bins: Some(&self.bins),
                paths: &self.paths,
                storage: &self.storage,
                warnings: &self.warnings,
            }
        }

        /// The same dependencies with no ffmpeg found — what an install without the
        /// sidecars sees.
        fn deps_without_ffmpeg(&self) -> Deps<'_> {
            Deps { bins: None, ..self.deps() }
        }

        /// Index a clip whose file is never opened. Enough for the list, favourite, policy
        /// and validation tests.
        fn add_row(&self, name: &str, started_at_ms: u64, duration_ms: u64, size_bytes: u64) -> i64 {
            self.store
                .insert_clip(&NewClip {
                    path: self.paths.clips_dir.join(name),
                    started_at_ms,
                    duration_ms,
                    size_bytes,
                    codec: "mpeg4".to_string(),
                })
                .unwrap()
        }

        /// Write a real, decodable clip into the clips directory and index it.
        fn add_real_clip(&self, name: &str, seconds: u32) -> (i64, PathBuf) {
            let path = self.paths.clips_dir.join(name);
            write_test_video(&self.bins, &path, seconds);
            let size_bytes = std::fs::metadata(&path).unwrap().len();
            let id = self
                .store
                .insert_clip(&NewClip {
                    path: path.clone(),
                    started_at_ms: 0,
                    duration_ms: u64::from(seconds) * 1_000,
                    size_bytes,
                    codec: "mpeg4".to_string(),
                })
                .unwrap();
            (id, path)
        }

        /// A **finished** session whose recorded file is a real, decodable video.
        ///
        /// The wall start and the media epoch are deliberately different numbers, and far
        /// apart: that is the entire reason the column exists, and a fixture with the two
        /// equal cannot tell one clock from the other.
        ///
        /// The segments and the recorded file live in **different** directories, which is the
        /// real layout (see the recorder's `session_file_path`: the concatenation is written
        /// into a sessions output directory, not among the segments it was built from).
        fn add_finished_session(&self, seconds: u32, media_epoch_ms: i64) -> (i64, PathBuf) {
            // Inside the same temporary root the fixture owns — `AppPaths` deliberately does
            // not publish the app-data directory itself.
            let root = self
                .paths
                .clips_dir
                .parent()
                .expect("the clips directory has a parent")
                .to_path_buf();
            let scratch_dir = root.join("sessions").join("session-1");
            let out_dir = root.join("sessions-out");
            std::fs::create_dir_all(&scratch_dir).unwrap();
            std::fs::create_dir_all(&out_dir).unwrap();
            let final_path = out_dir.join("session-1.mp4");
            write_test_video(&self.bins, &final_path, seconds);

            let id = self
                .store
                .start_session(
                    Some("Dota 2"),
                    localplay_store::SESSION_MODE_SESSION,
                    WALL_START_MS,
                    &scratch_dir.to_string_lossy(),
                    media_epoch_ms,
                )
                .unwrap();
            self.store
                .end_session(
                    id,
                    WALL_START_MS + i64::from(seconds) * 1_000,
                    Some(&final_path.to_string_lossy()),
                    std::fs::metadata(&final_path).unwrap().len() as i64,
                    // The fixture encodes `seconds` of video, so that IS its media length.
                    i64::from(seconds) * 1_000,
                )
                .unwrap();
            (id, final_path)
        }

        /// A session row that is still running, so it has no concatenated file. `mode` is
        /// which engine opened it — a buffer run and a full-session run both look like this
        /// until one of them stops.
        fn add_running_session(&self, mode: &str) -> (i64, PathBuf) {
            let dir = self
                .paths
                .clips_dir
                .parent()
                .expect("the clips directory has a parent")
                .join("sessions")
                .join(mode);
            std::fs::create_dir_all(&dir).unwrap();
            let id = self
                .store
                .start_session(None, mode, WALL_START_MS, &dir.to_string_lossy(), 0)
                .unwrap();
            (id, dir)
        }
    }

    /// Encode `seconds` of `testsrc2` into `path` with ffmpeg, exactly as a clip would be.
    ///
    /// `-g 10` at 10 fps puts a keyframe every second, so the file has the keyframe
    /// structure a real clip has and a stream-copy cut has something to work with.
    fn write_test_video(bins: &FfmpegBinaries, path: &Path, seconds: u32) {
        let mut cmd = Command::new(&bins.ffmpeg);
        cmd.args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size=320x180:rate=10:duration={seconds}"))
            .args(["-c:v", "mpeg4", "-q:v", "5", "-g", "10", "-pix_fmt", "yuv420p"])
            .arg(path);
        let out = run_with_timeout(cmd, Duration::from_secs(60)).expect("spawning ffmpeg");
        assert!(
            out.status.success(),
            "the test fixture could not be encoded: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(path.is_file(), "ffmpeg reported success but wrote no file");
    }

    /// Hash the video packets ffmpeg reads from `src` at `seek_ms`, for `dur_ms` if given.
    ///
    /// `-f md5` hashes packet *payloads*, so two equal hashes mean two byte-identical
    /// streams. Nothing is decoded or encoded on either side of the comparison, which is
    /// what makes it evidence about a stream copy rather than a proxy for one.
    fn stream_md5(bins: &FfmpegBinaries, src: &Path, seek_ms: u64, dur_ms: Option<u64>) -> String {
        let mut cmd = Command::new(&bins.ffmpeg);
        cmd.args(["-v", "error", "-y", "-ss"])
            .arg(format!("{:.3}", seek_ms as f64 / 1000.0))
            .arg("-i")
            .arg(src);
        if let Some(ms) = dur_ms {
            cmd.args(["-t"]).arg(format!("{:.3}", ms as f64 / 1000.0));
        }
        cmd.args(["-c", "copy", "-f", "md5", "-"]);
        let out = run_with_timeout(cmd, Duration::from_secs(60)).expect("spawning ffmpeg");
        assert!(
            out.status.success(),
            "the md5 run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    // -- list_clips ----------------------------------------------------------------------

    #[test]
    fn lists_clips_newest_first_by_creation_time_not_by_media_time() {
        let f = Fixture::new();
        // The older *row* carries the newer media timestamp. The store's own `list_clips`
        // orders by `started_at` and would put it first; the command must not, because
        // `started_at` restarts with the scratch directory and is not comparable across
        // runs.
        let older_row = f.add_row("older-row.mp4", 9_000, 1_000, 10);
        let newer_row = f.add_row("newer-row.mp4", 1_000, 1_000, 10);

        let clips = list_clips(&f.deps()).unwrap();

        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].id, newer_row, "the most recently indexed clip is first");
        assert_eq!(clips[1].id, older_row);
    }

    #[test]
    fn an_empty_index_lists_nothing() {
        assert!(list_clips(&Fixture::new().deps()).unwrap().is_empty());
    }

    #[test]
    fn a_clip_dto_carries_every_field_the_frontend_reads() {
        let f = Fixture::new();
        let id = f.add_row("fields.mp4", 1_234, 5_678, 9_101);

        let clips = list_clips(&f.deps()).unwrap();
        let clip = &clips[0];

        assert_eq!(clip.id, id);
        assert_eq!(clip.path, f.paths.clips_dir.join("fields.mp4").to_string_lossy());
        assert_eq!(clip.started_at_ms, 1_234);
        assert_eq!(clip.duration_ms, 5_678);
        assert_eq!(clip.size_bytes, 9_101);
        assert_eq!(clip.codec, "mpeg4");
        assert!(!clip.favourite, "clips are not favourites by default");
        assert!(clip.created_at_ms > 0, "the store stamps a wall-clock creation instant");
    }

    #[test]
    fn the_dto_json_matches_the_typescript_interface() {
        // `src/lib/types.ts` mirrors these structs by hand. Without this, a rename on
        // either side is discovered by a user looking at an empty list.
        let f = Fixture::new();
        f.add_row("shape.mp4", 1, 2, 3);

        let clip = serde_json::to_value(&list_clips(&f.deps()).unwrap()[0]).unwrap();
        let mut keys: Vec<&str> =
            clip.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "codec",
                "created_at_ms",
                "duration_ms",
                "favourite",
                "id",
                "path",
                "size_bytes",
                "started_at_ms"
            ]
        );

        let stats = serde_json::to_value(storage_stats(&f.deps()).unwrap()).unwrap();
        let mut keys: Vec<&str> =
            stats.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "bytes_after",
                "cap_bytes",
                "cap_met",
                "clip_count",
                "clips_dir",
                "favourite_bytes",
                "favourite_count",
                "max_age_days",
                "over_cap_by_bytes",
                "planned_deletions",
                "total_bytes",
                "warnings"
            ]
        );
    }

    #[test]
    fn a_command_error_serialises_with_a_machine_readable_code() {
        let value = serde_json::to_value(CommandError::clip_not_found(7)).unwrap();
        assert_eq!(value["code"], "clip_not_found");
        assert_eq!(value["message"], "no clip with id 7 is in the index");
    }

    // -- trim_clip -----------------------------------------------------------------------

    #[test]
    fn trim_rejects_an_empty_or_inverted_range() {
        let f = Fixture::new();
        let (id, src) = f.add_real_clip("ranges.mp4", 2);
        let src_size = std::fs::metadata(&src).unwrap().len();

        for (start, end) in [(1_000u64, 1_000u64), (1_500, 1_000), (500, 0)] {
            let err = trim_clip(&f.deps(), id, start, end).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidRange, "start={start} end={end}");
            assert!(
                err.message.contains("empty or inverted"),
                "the message must say what is wrong: {}",
                err.message
            );
        }

        // Rejected before ffmpeg ran: no file was written and no row was added.
        assert_eq!(list_clips(&f.deps()).unwrap().len(), 1, "nothing was indexed");
        assert_eq!(
            std::fs::read_dir(&f.paths.clips_dir).unwrap().count(),
            1,
            "the clips directory still holds only the parent"
        );
        assert_eq!(std::fs::metadata(&src).unwrap().len(), src_size);
    }

    #[test]
    fn trim_rejects_a_range_that_reaches_past_the_end_of_the_clip() {
        let f = Fixture::new();
        let (id, _src) = f.add_real_clip("short.mp4", 2);

        let err = trim_clip(&f.deps(), id, 1_000, 2_001).unwrap_err();

        assert_eq!(err.code, ErrorCode::OutOfRange);
        assert!(err.message.contains("2001ms"), "got: {}", err.message);
        assert!(err.message.contains("2000ms"), "the message names the real duration: {}", err.message);
    }

    #[test]
    fn trim_reports_an_unknown_clip() {
        let f = Fixture::new();
        let err = trim_clip(&f.deps(), 42, 0, 100).unwrap_err();
        assert_eq!(err.code, ErrorCode::ClipNotFound);
    }

    #[test]
    fn trim_writes_a_new_clip_beside_its_parent_and_indexes_it() {
        let f = Fixture::new();
        let (id, src) = f.add_real_clip("game.mp4", 4);
        let src_size = std::fs::metadata(&src).unwrap().len();

        let trimmed = trim_clip(&f.deps(), id, 1_500, 3_000).unwrap();

        let expected = f.paths.clips_dir.join("game.trim-1500-3000.mp4");
        assert_eq!(trimmed.path, expected.to_string_lossy());
        assert_eq!(expected.parent(), src.parent(), "the new clip sits beside the original");
        assert!(expected.is_file(), "the file must really be on disk");
        assert!(trimmed.duration_ms > 0, "the duration is probed from the file that was written");
        assert_eq!(trimmed.size_bytes, std::fs::metadata(&expected).unwrap().len());
        assert_eq!(trimmed.started_at_ms, 1_500, "its position on the parent's timeline");
        assert!(!trimmed.favourite);

        let clips = list_clips(&f.deps()).unwrap();
        assert_eq!(clips.len(), 2, "the trim is a clip of its own");
        assert_eq!(clips[0].id, trimmed.id, "and it is the newest row");
        assert!(clips.iter().any(|c| c.id == id), "the parent is still indexed");

        // The original is untouched: a trim never modifies the clip it came from.
        assert_eq!(std::fs::metadata(&src).unwrap().len(), src_size);
    }

    #[test]
    fn a_second_identical_trim_does_not_overwrite_the_first() {
        let f = Fixture::new();
        let (id, _src) = f.add_real_clip("twice.mp4", 4);

        let first = trim_clip(&f.deps(), id, 1_000, 2_000).unwrap();
        let second = trim_clip(&f.deps(), id, 1_000, 2_000).unwrap();

        assert_ne!(first.path, second.path, "the second trim must not clobber the first");
        assert_eq!(second.path, f.paths.clips_dir.join("twice.trim-1000-2000-2.mp4").to_string_lossy());
        assert!(Path::new(&first.path).is_file());
        assert_eq!(list_clips(&f.deps()).unwrap().len(), 3);
    }

    #[test]
    fn trim_is_a_stream_copy_not_a_re_encode() {
        // The evidence: the trimmed file's video packets are byte-identical to the packets
        // ffmpeg reads out of the same span of the parent. A re-encode cannot produce the
        // same bytes for the same frames — which the negative control at the end proves, so
        // that this assertion is known to be able to fail.
        let f = Fixture::new();
        let (id, src) = f.add_real_clip("copy.mp4", 4);

        let trimmed = trim_clip(&f.deps(), id, 1_500, 3_000).unwrap();
        let trimmed_path = PathBuf::from(&trimmed.path);

        // Both sides cover the same span of stream, measured from the file that was
        // written, so a stream copy that could not cut at exactly 3000ms still compares
        // like for like.
        let from_written_file = stream_md5(&f.bins, &trimmed_path, 0, None);
        let from_parent = stream_md5(&f.bins, &src, 1_500, Some(trimmed.duration_ms));
        assert_eq!(
            from_written_file, from_parent,
            "the trimmed packets must be the parent's own packets"
        );

        // Negative control: the same span, re-encoded, is a different byte stream.
        let re_encoded = f.paths.clips_dir.join("re-encoded.mp4");
        let mut cmd = Command::new(&f.bins.ffmpeg);
        cmd.args(["-v", "error", "-y", "-ss", "1.500", "-i"])
            .arg(&src)
            .args(["-t", "1.500", "-c:v", "mpeg4", "-q:v", "5", "-g", "10"])
            .arg(&re_encoded);
        let out = run_with_timeout(cmd, Duration::from_secs(60)).unwrap();
        assert!(out.status.success(), "the control encode failed");
        assert_ne!(
            from_parent,
            stream_md5(&f.bins, &re_encoded, 0, None),
            "a re-encode must not hash the same, or this test proves nothing"
        );
    }

    #[test]
    fn trim_can_cut_the_whole_clip() {
        // The upper bound is inclusive on purpose: a scrubber dragged to the end of the
        // clip asks for exactly `duration_ms`, and that is a legal range.
        let f = Fixture::new();
        let (id, _src) = f.add_real_clip("whole.mp4", 2);

        let trimmed = trim_clip(&f.deps(), id, 0, 2_000).unwrap();

        assert_eq!(trimmed.path, f.paths.clips_dir.join("whole.trim-0-2000.mp4").to_string_lossy());
        assert!(trimmed.duration_ms > 0);
    }

    // -- set_favourite -------------------------------------------------------------------

    #[test]
    fn set_favourite_flips_the_flag_and_reports_an_unknown_id() {
        let f = Fixture::new();
        let id = f.add_row("fav.mp4", 0, 1_000, 10);
        assert!(!list_clips(&f.deps()).unwrap()[0].favourite);

        let marked = set_favourite(&f.deps(), id, true).unwrap();
        assert_eq!(marked.id, id);
        assert!(marked.favourite);
        assert!(list_clips(&f.deps()).unwrap()[0].favourite, "the flag is persisted");

        let cleared = set_favourite(&f.deps(), id, false).unwrap();
        assert!(!cleared.favourite);
        assert!(!list_clips(&f.deps()).unwrap()[0].favourite);

        let err = set_favourite(&f.deps(), id + 1, true).unwrap_err();
        assert_eq!(err.code, ErrorCode::ClipNotFound, "a no-op update must not look like success");
    }

    // -- storage_stats -------------------------------------------------------------------

    #[test]
    fn storage_stats_reports_usage_the_cap_and_a_met_verdict() {
        let f = Fixture::with_storage(StorageConfigView {
            max_total_bytes: 1_000,
            max_age_days: 3_650,
        });
        f.add_row("a.mp4", 0, 1_000, 400);
        let favourite = f.add_row("b.mp4", 0, 1_000, 300);
        set_favourite(&f.deps(), favourite, true).unwrap();

        let stats = storage_stats(&f.deps()).unwrap();

        assert_eq!(stats.clip_count, 2);
        assert_eq!(stats.total_bytes, 700);
        assert_eq!(stats.favourite_count, 1);
        assert_eq!(stats.favourite_bytes, 300);
        assert_eq!(stats.cap_bytes, 1_000);
        assert_eq!(stats.max_age_days, 3_650);
        assert!(stats.cap_met, "700 bytes is inside a 1000 byte cap");
        assert_eq!(stats.over_cap_by_bytes, 0);
        assert_eq!(stats.planned_deletions, 0);
        assert_eq!(stats.bytes_after, 700);
        assert_eq!(stats.clips_dir, f.paths.clips_dir.to_string_lossy());
        assert!(stats.warnings.is_empty());
    }

    #[test]
    fn storage_stats_says_when_the_favourites_alone_exceed_the_cap() {
        // Spec §8.1: the manager stops rather than delete a favourite to satisfy a cap it
        // cannot meet. The panel has to be able to say why nothing will ever be freed.
        let f = Fixture::with_storage(StorageConfigView {
            max_total_bytes: 500,
            max_age_days: 3_650,
        });
        let favourite = f.add_row("big-favourite.mp4", 0, 1_000, 800);
        set_favourite(&f.deps(), favourite, true).unwrap();
        f.add_row("small.mp4", 0, 1_000, 100);

        let stats = storage_stats(&f.deps()).unwrap();

        assert!(!stats.cap_met);
        assert_eq!(stats.over_cap_by_bytes, 300, "800 bytes of favourites against a 500 cap");
        assert_eq!(stats.planned_deletions, 0, "evicting everything else cannot reach this cap");
    }

    #[test]
    fn storage_stats_plans_the_eviction_when_the_cap_is_reachable() {
        let f = Fixture::with_storage(StorageConfigView {
            max_total_bytes: 1_000,
            max_age_days: 3_650,
        });
        f.add_row("old.mp4", 0, 1_000, 600);
        f.add_row("new.mp4", 0, 1_000, 600);

        let stats = storage_stats(&f.deps()).unwrap();

        assert_eq!(stats.total_bytes, 1_200);
        assert!(stats.cap_met, "with no favourites in the way the cap is reachable");
        assert_eq!(stats.planned_deletions, 1, "one eviction brings 1200 bytes under the cap");
        assert_eq!(stats.bytes_after, 600);
    }

    #[test]
    fn storage_stats_applies_the_age_rule_against_the_clock_it_is_given() {
        let f = Fixture::with_storage(StorageConfigView {
            max_total_bytes: u64::MAX,
            max_age_days: 7,
        });
        f.add_row("recent.mp4", 0, 1_000, 100);
        let now = now_ms();

        let fresh = storage_stats_at(&f.deps(), now).unwrap();
        assert_eq!(fresh.planned_deletions, 0, "a clip indexed just now is not a week old");

        let day = 24 * 60 * 60 * 1_000;
        let later = storage_stats_at(&f.deps(), now + 30 * day).unwrap();
        assert_eq!(later.planned_deletions, 1, "30 days on, the 7 day limit selects it");
        assert!(later.cap_met, "the age rule does not make the cap unreachable");
        assert_eq!(later.bytes_after, 0);
    }

    #[test]
    fn storage_stats_carries_the_startup_warnings() {
        let mut f = Fixture::new();
        f.warnings = vec!["ffmpeg was not found".to_string()];

        assert_eq!(
            storage_stats(&f.deps()).unwrap().warnings,
            vec!["ffmpeg was not found".to_string()]
        );
    }

    // -- thumbnail -----------------------------------------------------------------------

    #[test]
    fn thumbnail_writes_a_jpeg_into_the_cache_and_reuses_it() {
        let f = Fixture::new();
        let (id, _src) = f.add_real_clip("framed.mp4", 4);

        let first = thumbnail(&f.deps(), id, 1_000).unwrap();

        assert!(!first.cached, "the first request has to run ffmpeg");
        assert_eq!(first.at_ms, 1_000);
        assert_eq!(first.clip_id, id);
        let path = PathBuf::from(&first.path);
        assert_eq!(path.parent().unwrap(), f.paths.thumbnails_dir, "the cache, not the clips dir");
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            format!("clip-{id}-1000.jpg")
        );
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..3], &[0xFF, 0xD8, 0xFF], "a JPEG starts with FFD8FF");
        assert!(bytes.len() > 200, "and is not an empty file: {} bytes", bytes.len());

        let second = thumbnail(&f.deps(), id, 1_000).unwrap();
        assert!(second.cached, "the second request is answered from the cache, not ffmpeg");
        assert_eq!(first.path, second.path);

        // A different frame is a different cache entry.
        let other = thumbnail(&f.deps(), id, 2_000).unwrap();
        assert!(!other.cached);
        assert_ne!(other.path, first.path);
        assert_eq!(std::fs::read_dir(&f.paths.thumbnails_dir).unwrap().count(), 2);
    }

    #[test]
    fn thumbnail_rejects_a_timestamp_past_the_end_of_the_clip() {
        let f = Fixture::new();
        let (id, _src) = f.add_real_clip("short.mp4", 2);

        let err = thumbnail(&f.deps(), id, 2_000).unwrap_err();

        assert_eq!(err.code, ErrorCode::OutOfRange);
        assert!(err.message.contains("2000ms"), "got: {}", err.message);
        // A frame from inside the clip is served. The guarantee this command gives is
        // "the timestamp names a position inside the clip", not "ffmpeg will find a frame
        // there": the last frame of a stream starts a frame interval before its nominal
        // duration ends, so a timestamp a few milliseconds short of the end can still fail
        // — see the note on `thumbnail`. The UI asks for a frame in the first second.
        assert!(thumbnail(&f.deps(), id, 1_000).is_ok());
    }

    #[test]
    fn thumbnail_reports_an_unknown_clip() {
        let f = Fixture::new();
        assert_eq!(thumbnail(&f.deps(), 7, 0).unwrap_err().code, ErrorCode::ClipNotFound);
    }

    #[test]
    fn a_thumbnail_ffmpeg_cannot_produce_names_the_clip_and_the_timestamp() {
        // A row pointing at something that is not a video: ffmpeg fails, and the message has
        // to say which clip and which frame — "ffmpeg had nothing to read" is otherwise
        // indistinguishable from a corrupt library.
        let f = Fixture::new();
        let broken = f.paths.clips_dir.join("truncated.mp4");
        std::fs::write(&broken, b"not a video at all").unwrap();
        let id = f
            .store
            .insert_clip(&NewClip {
                path: broken,
                started_at_ms: 0,
                duration_ms: 4_000,
                size_bytes: 16,
                codec: "mpeg4".to_string(),
            })
            .unwrap();

        let err = thumbnail(&f.deps(), id, 1_500).unwrap_err();

        assert_eq!(err.code, ErrorCode::Media);
        assert!(
            err.message.contains(&format!("clip #{id} at 1500ms")),
            "the message must name the clip and the frame: {}",
            err.message
        );
    }

    #[test]
    fn media_commands_fail_clearly_when_ffmpeg_is_missing() {
        // The shell opens without ffmpeg — the list and the storage panel need none — so
        // the two commands that do need it must fail with a code the UI can explain,
        // rather than panicking or taking the window down.
        let f = Fixture::new();
        let id = f.add_row("no-ffmpeg.mp4", 0, 1_000, 10);
        let deps = f.deps_without_ffmpeg();

        assert_eq!(trim_clip(&deps, id, 0, 500).unwrap_err().code, ErrorCode::FfmpegUnavailable);
        assert_eq!(thumbnail(&deps, id, 100).unwrap_err().code, ErrorCode::FfmpegUnavailable);

        assert_eq!(list_clips(&deps).unwrap().len(), 1, "listing needs no ffmpeg");
        assert_eq!(storage_stats(&deps).unwrap().clip_count, 1);
        assert!(set_favourite(&deps, id, true).is_ok(), "nor does favouriting");
    }

    // -- delete_clip ---------------------------------------------------------------------

    #[test]
    fn delete_removes_the_row_and_then_the_file() {
        let f = Fixture::new();
        let (id, path) = f.add_real_clip("gone.mp4", 2);
        let size_bytes = std::fs::metadata(&path).unwrap().len();

        let outcome = delete_clip(&f.deps(), id).unwrap();

        assert!(outcome.row_deleted);
        assert!(outcome.file_removed);
        assert_eq!(outcome.orphaned_path, None);
        assert!(!outcome.already_missing);
        assert_eq!(outcome.bytes_reclaimed, size_bytes);
        assert!(!path.exists(), "the file is unlinked");
        assert!(list_clips(&f.deps()).unwrap().is_empty(), "and the row is gone");
    }

    #[test]
    fn delete_reports_an_orphan_when_the_file_cannot_be_unlinked() {
        // A directory where a clip file should be: `remove_file` refuses it on every
        // platform, which is the only portable way to make an unlink fail on purpose.
        // The state this leaves is the one spec §8.2 accepts as recoverable, and the one
        // the UI must be able to name.
        let f = Fixture::new();
        let not_a_file = f.paths.clips_dir.join("not-a-file.mp4");
        std::fs::create_dir(&not_a_file).unwrap();
        let id = f
            .store
            .insert_clip(&NewClip {
                path: not_a_file.clone(),
                started_at_ms: 0,
                duration_ms: 1_000,
                size_bytes: 10,
                codec: "mpeg4".to_string(),
            })
            .unwrap();

        let outcome = delete_clip(&f.deps(), id).unwrap();

        let expected = not_a_file.to_string_lossy().into_owned();
        assert!(outcome.row_deleted, "the row delete is committed before the unlink is tried");
        assert!(!outcome.file_removed);
        assert_eq!(outcome.orphaned_path.as_deref(), Some(expected.as_str()));
        assert_eq!(outcome.bytes_reclaimed, 0);
        assert!(not_a_file.exists(), "the orphaned path is reported, not hidden");
        assert!(
            list_clips(&f.deps()).unwrap().is_empty(),
            "the row went first and stayed gone: that ordering is the property"
        );
    }

    #[test]
    fn delete_reports_a_row_whose_file_was_already_gone() {
        let f = Fixture::new();
        let id = f.add_row("missing.mp4", 0, 1_000, 10);

        let outcome = delete_clip(&f.deps(), id).unwrap();

        assert!(outcome.row_deleted);
        assert!(!outcome.file_removed);
        assert!(outcome.already_missing);
        assert_eq!(outcome.orphaned_path, None);
        assert_eq!(outcome.bytes_reclaimed, 0);
    }

    #[test]
    fn delete_of_an_unknown_id_unlinks_nothing() {
        // Spec §8.2: no file is deleted unless a committed database row names it. So an id
        // with no row must leave any file of that name exactly where it is.
        let f = Fixture::new();
        let stray = f.paths.clips_dir.join("stray.mp4");
        std::fs::write(&stray, b"not indexed, and therefore not this command's to delete").unwrap();

        let outcome = delete_clip(&f.deps(), 999).unwrap();

        assert!(!outcome.row_deleted);
        assert!(!outcome.file_removed);
        assert!(stray.is_file(), "an unindexed file is not deleted");
    }

    #[test]
    fn delete_takes_that_clip_cached_thumbnails_with_it() {
        let f = Fixture::new();
        let (id, _src) = f.add_real_clip("thumbed.mp4", 2);
        let thumb = thumbnail(&f.deps(), id, 1_000).unwrap();
        let thumb_path = PathBuf::from(&thumb.path);
        assert!(thumb_path.is_file());

        // Another clip's cache entry, whose id shares a prefix with this one's.
        let neighbour = f.paths.thumbnails_dir.join(thumbnail_file_name(id + 100, 1_000));
        std::fs::write(&neighbour, b"another clip's frame").unwrap();

        let outcome = delete_clip(&f.deps(), id).unwrap();

        assert_eq!(outcome.thumbnails_removed, 1);
        assert!(!thumb_path.exists(), "the deleted clip's cache entry goes with it");
        assert!(neighbour.is_file(), "a neighbouring clip's cache entry stays");
    }

    // -- pure helpers --------------------------------------------------------------------

    #[test]
    fn the_trim_output_name_carries_the_range_it_was_cut_from() {
        assert_eq!(
            trim_output_path(Path::new("/clips/game.mp4"), 1_500, 3_000),
            PathBuf::from("/clips/game.trim-1500-3000.mp4")
        );
        // The extension is preserved (the muxer flags `trim_lossless` passes are only
        // accepted by an ISO-BMFF muxer, so the extension is carried, never assumed).
        assert_eq!(
            trim_output_path(Path::new("clip.mkv"), 0, 10),
            PathBuf::from("clip.trim-0-10.mkv")
        );
        // A stem containing dots keeps every part but the last extension.
        assert_eq!(
            trim_output_path(Path::new("/clips/2026.09.23 game.mp4"), 5, 6),
            PathBuf::from("/clips/2026.09.23 game.trim-5-6.mp4")
        );
    }

    #[test]
    fn a_free_output_name_steps_around_the_files_already_on_disk() {
        let f = Fixture::new();
        let candidate = f.paths.clips_dir.join("taken.trim-0-1000.mp4");
        std::fs::write(&candidate, b"a previous trim").unwrap();

        let chosen = free_output_path(candidate.clone()).unwrap();
        assert_eq!(chosen, f.paths.clips_dir.join("taken.trim-0-1000-2.mp4"));

        // With that one taken too, the next number is used — never an overwrite.
        std::fs::write(&chosen, b"and another").unwrap();
        assert_eq!(
            free_output_path(candidate.clone()).unwrap(),
            f.paths.clips_dir.join("taken.trim-0-1000-3.mp4")
        );
        assert_eq!(std::fs::read(&chosen).unwrap(), b"and another", "nothing was clobbered");
    }

    #[test]
    fn the_thumbnail_cache_name_is_derived_from_the_clip_and_the_frame() {
        assert_eq!(thumbnail_file_name(1, 0), "clip-1-0.jpg");
        assert_eq!(thumbnail_file_name(12, 1_500), "clip-12-1500.jpg");
    }

    // -- recording -----------------------------------------------------------------------

    /// A recorder host over the fixture's temporary directory.
    ///
    /// `with_ffmpeg` is false for the "no sidecars installed" case; when it is true the
    /// host holds the real binaries, but no test here ever lets it *start* a recording
    /// that gets as far as capturing — see `a_start_the_engine_refuses_fails_before_any_capture_starts`.
    fn host(f: &Fixture, with_ffmpeg: bool) -> RecorderHost {
        let app_data_dir = f.paths.db_path.parent().expect("the db lives in the app dir");
        RecorderHost::new(app_data_dir.to_path_buf(), with_ffmpeg.then(|| f.bins.clone()))
    }

    #[test]
    fn recording_status_is_the_idle_status_before_anything_is_started() {
        let f = Fixture::new();
        let status = host(&f, true).status().unwrap();

        assert_eq!(status, RecordingStatusDto::from(RecorderStatus::stopped()));
        assert!(!status.running, "a shell that has never recorded reports not running");
        assert_eq!(status.configured_fps, 0, "and claims no configured rate");
        assert_eq!(status.effective_fps, 0, "nor a rate in use");
        assert_eq!(status.error, None);
    }

    #[test]
    fn a_start_without_ffmpeg_fails_with_the_code_the_ui_explains() {
        let f = Fixture::new();
        let host = host(&f, false);

        let err = host.start(&RecordingConfig::example().unwrap()).unwrap_err();

        assert_eq!(err.code, ErrorCode::FfmpegUnavailable);
        assert!(err.message.contains("ffmpeg was not found"), "got: {}", err.message);
        assert!(!host.status().unwrap().running, "and nothing was started");
    }

    #[test]
    fn a_start_the_engine_refuses_fails_before_any_capture_starts() {
        // Why this is the only test that reaches `RecorderHost::start` with real binaries:
        // the engine resolves the encoder — codec, then vendor — *before* it creates the
        // capture backend, and on Windows that backend is a live Windows Graphics Capture
        // session. This test therefore fails the start at the codec, on every platform,
        // without a capture session ever existing. A test that let a start succeed here
        // would record the screen of whoever ran the suite.
        let f = Fixture::new();
        let host = host(&f, true);
        let mut settings = RecordingConfig::example().unwrap();
        settings.encode.codec = "vp9".to_string();

        let err = host.start(&settings).unwrap_err();

        assert_eq!(err.code, ErrorCode::Recording);
        assert!(
            err.message.contains("unsupported encode.codec"),
            "the message must name the setting, through the command error: {}",
            err.message
        );
        assert!(!host.status().unwrap().running, "a failed start leaves nothing running");
    }

    /// **The integration test for the feature the background half exists for.**
    ///
    /// One press of the configured chord calls `RecorderHost::clip_now` — the same call the
    /// window's Save clip button and the CLI's driver loop make — and that call really does
    /// produce a clip: a real encoder child process, a real ring on disk, a real lossless
    /// splice and a real row in the index.
    ///
    /// The trigger is an in-process function call. **No key, button or mouse event is
    /// synthesised, simulated or injected anywhere in this test or in the code it drives** —
    /// what needs Windows is `RegisterHotKey` (the chord itself), and everything on either
    /// side of it is what this test covers. The capture source is the engine's synthetic
    /// stub, so the suite opens no capture session on the host's display, and the encoder is
    /// libx264, because a CI host has no GPU encoder.
    #[test]
    fn one_hotkey_press_writes_exactly_one_clip_through_the_recorder() {
        const PRE_SECONDS: u64 = 2;
        const POST_SECONDS: u64 = 1;

        let f = Fixture::new();
        let host = host(&f, true);

        // The example config, shrunk to something that runs in seconds: a 2s pre-roll, a 1s
        // post-roll, 10fps, and the capture backend's own size.
        let mut settings = RecordingConfig::example().unwrap();
        settings.buffer.pre_seconds = PRE_SECONDS;
        settings.buffer.post_seconds = POST_SECONDS;
        settings.buffer.segment_time = 1;
        settings.encode.fps = 10;
        settings.encode.output_size = String::new();
        settings.storage.max_total_bytes = u64::MAX;
        settings.storage.max_age_days = 3_650;

        let started = host.start_for_test_with_stub_sources(&settings).unwrap();
        assert!(started.running, "the engine is capturing before any press");

        // Wait for the ring to hold the whole window — the same wait the CLI's self-test
        // does before its trigger, and for the same reason: the engine refuses a trigger
        // that asks for footage it does not have yet.
        let need_ms = (PRE_SECONDS + POST_SECONDS) * 1_000;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let status = host.status().unwrap();
            if status.span_ms >= need_ms {
                break;
            }
            assert!(
                status.running,
                "the engine stopped while the ring was filling: {:?}",
                status.error
            );
            assert!(
                Instant::now() < deadline,
                "the ring never held {need_ms}ms of media (last: {}ms)",
                status.span_ms
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        // THE PRESS. `on_hotkey_press` is the function the hotkey thread calls; the closure
        // is the call the button makes.
        let outcome = crate::background::on_hotkey_press(|| host.clip_now());

        let status = host.status().unwrap();
        assert!(outcome.saved_a_clip(), "the press saved no clip: {outcome:?}");
        assert_eq!(status.clips, 1, "the engine counted exactly one clip, not two");

        // One file on disk, named as the trigger path names them.
        let clips: Vec<PathBuf> = std::fs::read_dir(&f.paths.clips_dir)
            .expect("the clips directory exists")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("clip-") && name.ends_with(".mp4"))
            })
            .collect();
        assert_eq!(clips.len(), 1, "expected exactly one clip file, got {clips:?}");

        // And it is real media of the configured window, produced by the real splice.
        let info = MediaInfo::probe(&f.bins, &clips[0]).expect("the clip is readable media");
        let expected_ms = (PRE_SECONDS + POST_SECONDS) * 1_000;
        assert!(
            (info.duration_ms as i64 - expected_ms as i64).abs() <= 800,
            "clip duration {}ms is not the configured window ({expected_ms}ms ± the segment \
             grid)",
            info.duration_ms
        );
        assert_eq!(
            info.video.as_ref().expect("a video stream").codec,
            "h264",
            "the software encoder is libx264 in a test build"
        );

        // One row in the index — the row `clip_now` wrote, in the store this shell reads.
        let rows = f.store.list_clips().unwrap();
        assert_eq!(rows.len(), 1, "the clip was indexed exactly once");
        assert_eq!(rows[0].path, clips[0], "and the row names the file on disk");

        // A stop flushes the encoder and closes the capture session.
        let stopped = host.stop().unwrap();
        assert!(!stopped.running);
    }

    #[test]
    fn stopping_nothing_is_idempotent_and_says_so() {
        let f = Fixture::new();
        let host = host(&f, true);

        let first = host.stop().unwrap();
        let second = host.stop().unwrap();

        assert_eq!(first, second, "a stop with nothing running is a value, not a failure");
        assert!(!first.running);
        assert_eq!(first.error, None);
    }

    #[test]
    fn a_clip_with_nothing_recording_is_refused_rather_than_waited_for() {
        let f = Fixture::new();
        let host = host(&f, true);

        let err = host.clip_now().unwrap_err();

        assert_eq!(err.code, ErrorCode::Recording);
        assert!(
            err.message.contains("nothing is recording"),
            "the message must say why there is no clip: {}",
            err.message
        );
    }

    #[test]
    fn the_recording_dto_json_matches_the_typescript_interface() {
        // `src/lib/types.ts` mirrors these two structs by hand, like the clip and storage
        // DTOs above: without this, a rename on either side is discovered by a user
        // watching a status readout that never moves.
        let status = serde_json::to_value(RecordingStatusDto::from(RecorderStatus::stopped())).unwrap();
        let mut keys: Vec<&str> = status.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "bytes",
                "clips",
                "configured_fps",
                "drift_ms",
                "dropped",
                "dropped_audio",
                "effective_fps",
                "error",
                "fps",
                "frames",
                "running",
                "segments",
                "skipped",
                "span_ms"
            ]
        );

        let clip = serde_json::to_value(RecordedClipDto {
            id: Some(3),
            path: "/clips/clip-1.mp4".to_string(),
            duration_ms: 12_000,
            size_bytes: 1_024,
            codec: "libx264".to_string(),
            started_at_ms: 4_000,
        })
        .unwrap();
        let mut keys: Vec<&str> = clip.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            ["codec", "duration_ms", "id", "path", "size_bytes", "started_at_ms"]
        );
        assert_eq!(clip["id"], 3, "an indexed clip carries its row id");
    }

    #[test]
    fn the_asset_roots_are_the_clips_directory_and_the_thumbnail_cache() {
        // What `lib.rs` hands to the runtime asset-protocol scope: playback is scoped to
        // the clips directory, and the thumbnails directory is the cache served alongside
        // it. Nothing else in the filesystem is reachable from the webview.
        let dir = PathBuf::from("/app-data/localplay");
        let paths = AppPaths::resolve(&dir, "");
        assert_eq!(paths.asset_roots(), [Path::new("/app-data/localplay/clips"), Path::new("/app-data/localplay/thumbnails")]);
        assert_eq!(paths.db_path, dir.join("localplay.db"), "the same index file the CLI uses");

        // A configured clips directory is honoured, and is still the first asset root.
        let custom = AppPaths::resolve(&dir, "/mnt/games/localplay-clips");
        assert_eq!(custom.clips_dir, PathBuf::from("/mnt/games/localplay-clips"));
        assert_eq!(custom.asset_roots()[0], Path::new("/mnt/games/localplay-clips"));
    }

    // -- sessions ------------------------------------------------------------------------

    /// The wall clock the session fixtures start at: deliberately enormous, so that a
    /// timeline offset computed against the wall start instead of the media epoch is
    /// unmistakably wrong rather than merely imprecise.
    const WALL_START_MS: i64 = 1_700_000_000_000;

    #[test]
    fn the_session_dto_json_matches_the_typescript_interface() {
        // `src/lib/types.ts` mirrors these by hand. Without this, a rename on either side is
        // discovered by a user looking at an empty session list.
        let f = Fixture::new();
        let (session_id, _) = f.add_finished_session(2, 1_000);

        let dto = serde_json::to_value(session_detail(&f.deps(), session_id).unwrap()).unwrap();
        let mut keys: Vec<&str> = dto.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "duration_ms",
                "ended_at_ms",
                "favourite",
                "final_path",
                "game",
                "id",
                "mode",
                "scratch_dir",
                "size_bytes",
                "started_at_ms"
            ]
        );
        assert!(
            !keys.contains(&"media_epoch_ms"),
            "the epoch stays behind the IPC boundary. Offsets arrive computed, and publishing \
             the anchor as well would invite the frontend to subtract a second time — which is \
             the cross-clock defect the column exists to fix"
        );

        f.store
            .insert_event(&localplay_store::NewEvent {
                session_id: Some(session_id),
                kind: "kill".to_string(),
                at_ms: 1_500,
                payload: Some("{\"source\":\"lol\"}".to_string()),
                clip_id: None,
            })
            .unwrap();
        let events = session_events(&f.deps(), session_id).unwrap();
        let event = serde_json::to_value(&events[0]).unwrap();
        let mut keys: Vec<&str> = event.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, ["clip_id", "id", "kind", "offset_ms", "payload"]);

        let outcome = delete_session(&f.deps(), session_id).unwrap();
        let value = serde_json::to_value(&outcome).unwrap();
        let mut keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            ["bytes_reclaimed", "final_file_removed", "id", "orphaned", "row_deleted", "scratch_removed"]
        );
    }

    #[test]
    fn a_session_timeline_is_measured_from_the_media_epoch_not_the_wall_clock() {
        // The defect this pins, at the IPC boundary: `offset_ms` used to be
        // `events.at - sessions.started_at` — media time minus wall time. With the two clocks
        // 1.7e12 ms apart, subtracting the wrong one is not a rounding error, it is a
        // different number entirely, and this is what the scrubber plotted.
        let f = Fixture::new();
        let (session_id, _) = f.add_finished_session(4, 1_000);
        for (kind, at_ms) in [("kill", 1_200u64), ("death", 4_000)] {
            f.store
                .insert_event(&localplay_store::NewEvent {
                    session_id: Some(session_id),
                    kind: kind.to_string(),
                    at_ms,
                    payload: None,
                    clip_id: None,
                })
                .unwrap();
        }

        let events = session_events(&f.deps(), session_id).unwrap();
        assert_eq!(
            events.iter().map(|e| (e.kind.as_str(), e.offset_ms)).collect::<Vec<_>>(),
            vec![("kill", 200), ("death", 3_000)],
            "offset = at - media_epoch_ms (1000ms), not at - the wall start"
        );
    }

    #[test]
    fn an_empty_timeline_and_a_missing_session_are_different_answers() {
        // A session nobody tagged has no markers — an empty list. A session that does not
        // exist is an error. Conflating the two would show an empty scrubber for a typo.
        let f = Fixture::new();
        let (session_id, _) = f.add_finished_session(1, 0);
        assert!(session_events(&f.deps(), session_id).unwrap().is_empty());

        let err = session_events(&f.deps(), 9_999).expect_err("no such session");
        assert_eq!(err.code, ErrorCode::SessionNotFound);
        assert_eq!(session_detail(&f.deps(), 9_999).unwrap_err().code, ErrorCode::SessionNotFound);
        assert_eq!(
            set_session_favourite(&f.deps(), 9_999, true).unwrap_err().code,
            ErrorCode::SessionNotFound
        );
    }

    #[test]
    fn a_favourite_can_be_set_and_cleared_and_is_visible_in_the_list() {
        let f = Fixture::new();
        let (session_id, _) = f.add_finished_session(1, 0);
        assert!(!list_sessions(&f.deps()).unwrap()[0].favourite);

        assert!(set_session_favourite(&f.deps(), session_id, true).unwrap().favourite);
        assert!(list_sessions(&f.deps()).unwrap()[0].favourite, "the list reflects it");

        assert!(!set_session_favourite(&f.deps(), session_id, false).unwrap().favourite);
    }

    #[test]
    fn extracting_a_clip_from_a_running_session_is_refused() {
        // Its scratch directory is being written into, so the file a range would be measured
        // against is not the file that would be read.
        let f = Fixture::new();
        let (id, _) = f.add_running_session(localplay_store::SESSION_MODE_SESSION);

        let err = extract_clip(&f.deps(), id, 0, 500).expect_err("a running session has no file");
        assert_eq!(err.code, ErrorCode::SessionStillRecording);
        assert!(err.message.contains("still recording"), "{}", err.message);

        // Deleting it is refused for the same reason, and by the same code.
        let err = delete_session(&f.deps(), id).expect_err("a running session is not deletable");
        assert_eq!(err.code, ErrorCode::SessionStillRecording);
        assert!(f.store.get_session(id).unwrap().is_some(), "and the row is untouched");
    }

    #[test]
    fn a_buffer_session_has_no_file_to_extract_from() {
        // Buffer mode keeps a rolling ring rather than one session file, so there is nothing
        // for a range to be measured against. The message says which mode it was.
        let f = Fixture::new();
        let (id, dir) = f.add_running_session(localplay_store::SESSION_MODE_BUFFER);
        f.store
            .end_session(id, WALL_START_MS + 1_000, None, 0, 0)
            .unwrap();

        let err = extract_clip(&f.deps(), id, 0, 500).expect_err("no concatenated file");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("buffer"), "{}", err.message);
        assert!(dir.exists(), "the refusal changes nothing on disk");
    }

    #[test]
    fn extracting_a_clip_from_a_finished_session_writes_and_indexes_it() {
        let f = Fixture::new();
        let (session_id, _) = f.add_finished_session(4, 1_000);

        let clip = extract_clip(&f.deps(), session_id, 1_000, 2_000).expect("the extract");
        assert!(PathBuf::from(&clip.path).is_file(), "the clip is on disk: {}", clip.path);
        assert!(clip.size_bytes > 0);
        assert!(
            (500..=1_500).contains(&clip.duration_ms),
            "a stream copy cuts on keyframes, so the range is approximate, but it must be \
             near the second that was asked for: got {}ms",
            clip.duration_ms
        );
        // The new row is placed on the *session's* media timeline, so a clip extracted at a
        // marker sits at that marker: epoch 1000 + start 1000.
        assert_eq!(clip.started_at_ms, 2_000, "positioned on the session's own timeline");
        assert_eq!(
            list_clips(&f.deps()).unwrap().iter().filter(|c| c.id == clip.id).count(),
            1,
            "and it is in the library, not merely on disk"
        );
    }

    #[test]
    fn extracting_out_of_range_or_inverted_is_refused_before_ffmpeg_runs() {
        let f = Fixture::new();
        let (session_id, _) = f.add_finished_session(2, 0);

        let err = extract_clip(&f.deps(), session_id, 500, 500).expect_err("an empty range");
        assert_eq!(err.code, ErrorCode::InvalidRange);
        assert!(err.message.contains("session #"), "the message names what it refused: {}", err.message);

        let err = extract_clip(&f.deps(), session_id, 1_500, 900_000).expect_err("past the end");
        assert_eq!(err.code, ErrorCode::OutOfRange);
    }

    #[test]
    fn deleting_a_finished_session_removes_its_row_its_segments_and_its_file() {
        let f = Fixture::new();
        let (id, final_path) = f.add_finished_session(1, 0);
        // What the concatenation leaves behind in the scratch directory.
        let scratch = PathBuf::from(&f.store.get_session(id).unwrap().unwrap().scratch_dir);
        std::fs::write(scratch.join("seg-000001.mp4"), b"pretend segment").unwrap();

        let outcome = delete_session(&f.deps(), id).unwrap();
        assert!(outcome.row_deleted);
        assert!(outcome.scratch_removed, "the segments go");
        assert!(outcome.final_file_removed, "and the recorded file");
        assert!(outcome.orphaned.is_empty(), "nothing was left behind: {:?}", outcome.orphaned);
        assert!(!scratch.exists());
        assert!(!final_path.exists());
        assert!(outcome.bytes_reclaimed > 0, "the bytes that were freed are reported");
        assert!(f.store.get_session(id).unwrap().is_none(), "the row is gone");

        // A second delete is not an error: it says nothing was deleted, so a UI racing a
        // retention pass can refresh truthfully. Same contract as `delete_clip`.
        let again = delete_session(&f.deps(), id).unwrap();
        assert!(!again.row_deleted);
        assert_eq!(again.bytes_reclaimed, 0);
    }
}

