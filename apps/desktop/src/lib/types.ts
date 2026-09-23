/**
 * The shapes the Rust command layer returns, mirrored by hand from
 * `src-tauri/src/commands.rs`.
 *
 * The mirroring is checked from the other side: `commands.rs` has a test
 * (`the_dto_json_matches_the_typescript_interface`) that asserts the exact JSON key set of
 * every DTO here, so a rename on either side fails a Rust test rather than producing an
 * empty list in a window nobody is watching.
 *
 * `null` is used where Rust returns `Option<T>`: serde serialises `None` as `null`, not as
 * an absent key, so these are `| null` rather than optional properties.
 */

/** One clip as the index records it. */
export interface ClipDto {
  id: number;
  /** Absolute path on disk. Playback goes through the asset protocol, never this string. */
  path: string;
  /** Media-time position of the clip's first frame. Not comparable across captures. */
  started_at_ms: number;
  duration_ms: number;
  size_bytes: number;
  codec: string;
  favourite: boolean;
  /** Wall-clock instant the row was written, ms since the Unix epoch. */
  created_at_ms: number;
}

/** The storage panel's data. */
export interface StorageStats {
  clip_count: number;
  total_bytes: number;
  favourite_count: number;
  favourite_bytes: number;
  cap_bytes: number;
  max_age_days: number;
  /** `false` means the favourites alone exceed the cap and no pass can meet it. */
  cap_met: boolean;
  over_cap_by_bytes: number;
  planned_deletions: number;
  bytes_after: number;
  clips_dir: string;
  /** Problems found at startup, such as a missing ffmpeg. */
  warnings: string[];
}

/** Where a clip's cached thumbnail is. */
export interface ThumbnailRef {
  clip_id: number;
  path: string;
  at_ms: number;
  cached: boolean;
}

/** What a delete did, in the vocabulary of spec §8.2. */
export interface DeleteOutcome {
  id: number;
  row_deleted: boolean;
  file_removed: boolean;
  /** Set when the row went but the file could not be unlinked. */
  orphaned_path: string | null;
  already_missing: boolean;
  bytes_reclaimed: number;
  thumbnails_removed: number;
}

/** The machine-readable half of a command failure. */
export type ErrorCode =
  | 'clip_not_found'
  | 'invalid_range'
  | 'out_of_range'
  | 'invalid_input'
  | 'ffmpeg_unavailable'
  | 'store'
  | 'media'
  | 'io';

/** A structured command failure, as `CommandError` serialises it. */
export interface CommandError {
  code: ErrorCode;
  message: string;
}

/** A trim selection, in milliseconds from the start of a clip. */
export interface TrimRange {
  startMs: number;
  endMs: number;
}
