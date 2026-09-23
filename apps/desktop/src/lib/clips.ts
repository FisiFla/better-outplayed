/**
 * The clip list's view model: the store's rows turned into what the list and the player
 * need. Pure, so the mapping — including the two places it can quietly go wrong, the
 * basename of a Windows path and the choice of thumbnail frame — is tested.
 */

import { formatBytes, formatDateTime, formatDuration } from './time';
import type { ClipDto, DeleteOutcome, TrimRange } from './types';

/** One row of the clip list. */
export interface ClipView {
  id: number;
  /** The file's name, which is what the list shows: the directory is noise there. */
  name: string;
  /** The full path, for the detail view and for error messages. */
  path: string;
  /** `asset:` URL for the `<video>` element, built by [`ClipSource.assetUrl`]. */
  assetUrl: string;
  durationMs: number;
  durationLabel: string;
  sizeBytes: number;
  sizeLabel: string;
  codec: string;
  favourite: boolean;
  createdAtMs: number;
  createdAtLabel: string;
}

/**
 * The last path component of `path`.
 *
 * Both separators are handled: the application runs on Windows and indexes paths written
 * there, while the development host is macOS, and a list of clips whose names all read as
 * the full path because the split only knew about `/` is exactly the sort of thing a
 * window-less test suite has to catch instead of an eye.
 */
export function basename(path: string): string {
  const parts = path.split(/[\\/]/);
  const last = parts[parts.length - 1];
  return last !== undefined && last !== '' ? last : path;
}

/**
 * The frame to thumbnail a clip at.
 *
 * One second in, or the middle of anything shorter than that. Deliberately not the last
 * frame: a stream's final frame starts up to one frame interval before its nominal
 * duration ends, so asking for a frame a few milliseconds from the end can legitimately
 * read nothing at all (see the note on `thumbnail` in `src-tauri/src/commands.rs`). One
 * second in is past the first keyframe of every clip this application writes and nowhere
 * near the end.
 */
export function thumbnailAtMs(durationMs: number): number {
  if (!Number.isFinite(durationMs) || durationMs <= 0) return 0;
  const oneSecond = 1_000;
  return durationMs > oneSecond ? oneSecond : Math.floor(durationMs / 2);
}

/** Map one row. `assetUrl` is injected so this stays free of the Tauri API. */
export function toClipView(clip: ClipDto, assetUrl: (path: string) => string): ClipView {
  return {
    id: clip.id,
    name: basename(clip.path),
    path: clip.path,
    assetUrl: assetUrl(clip.path),
    durationMs: clip.duration_ms,
    durationLabel: formatDuration(clip.duration_ms),
    sizeBytes: clip.size_bytes,
    sizeLabel: formatBytes(clip.size_bytes),
    codec: clip.codec,
    favourite: clip.favourite,
    createdAtMs: clip.created_at_ms,
    createdAtLabel: formatDateTime(clip.created_at_ms),
  };
}

/**
 * Map a whole list, preserving its order.
 *
 * Order is not re-sorted here. `list_clips` already returns clips newest first, ordered by
 * the wall-clock instant the row was written — and it is the Rust side that owns that
 * decision, because it is the side that knows `clips.started_at` is not comparable across
 * captures. Sorting again here would be a second, silently different opinion.
 */
export function toClipViews(clips: ClipDto[], assetUrl: (path: string) => string): ClipView[] {
  return clips.map((clip) => toClipView(clip, assetUrl));
}

/** Which clip a list refresh should leave selected. */
export function resolveSelection(
  views: ClipView[],
  selectedId: number | null,
): number | null {
  if (views.length === 0) return null;
  if (selectedId !== null && views.some((view) => view.id === selectedId)) return selectedId;
  // The selected clip is gone (deleted, or the index was reloaded): fall to the newest.
  return views[0]?.id ?? null;
}

/**
 * What to tell the user after `delete_clip`, in terms of what actually happened.
 *
 * The four outcomes are the four states the Rust command can report, and three of them are
 * not "it went fine": a row whose file could not be unlinked leaves a file the index no
 * longer names (spec §8.2 — recoverable, but only if it is said out loud rather than
 * reported as a clean delete), and an id with no row deletes nothing at all.
 */
export function describeDelete(outcome: DeleteOutcome, name: string): string {
  if (!outcome.row_deleted) {
    return `${name} was no longer in the index, so nothing was deleted.`;
  }
  if (outcome.orphaned_path !== null) {
    return (
      `${name} was removed from the index, but its file could not be deleted and is ` +
      `left on disk at ${outcome.orphaned_path}. It is no longer managed by the storage policy.`
    );
  }
  if (outcome.already_missing) {
    return `${name} was removed from the index; its file was already gone.`;
  }
  return `${name} deleted, freeing ${formatBytes(outcome.bytes_reclaimed)}.`;
}

/**
 * The two durations a trim report has to show: what was asked for, and what was written.
 *
 * They are not always the same. A stream copy cuts where the stream allows, so `trim_clip`
 * reports the probed length of the file rather than echoing the request — and the only
 * honest thing to do with two different numbers is show both.
 */
export function describeTrim(requested: TrimRange, written: ClipDto): string {
  const asked = Math.max(0, requested.endMs - requested.startMs);
  const got = written.duration_ms;
  const name = basename(written.path);

  // Within a couple of frames at ordinary frame rates, the two are the same number as far
  // as anyone reading this is concerned; beyond that the difference is worth stating.
  if (Math.abs(asked - got) <= TRIM_MATCH_TOLERANCE_MS) {
    return `Trimmed to ${name} (${formatDuration(got)}), losslessly remuxed and indexed as clip #${written.id}.`;
  }
  return (
    `Trimmed to ${name}: asked for ${formatDuration(asked)}, the file is ` +
    `${formatDuration(got)} — a stream copy cuts where the stream allows. Indexed as ` +
    `clip #${written.id}.`
  );
}

/** How close two durations have to be before the trim report calls them equal. */
const TRIM_MATCH_TOLERANCE_MS = 50;
