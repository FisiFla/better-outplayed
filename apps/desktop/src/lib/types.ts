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

/** The live recording status, as `recording_status` returns it. */
export interface RecordingStatus {
  /** False also means "never started": there is one status shape, not two. */
  running: boolean;
  /** Video frames submitted to the encoder this session. */
  frames: number;
  /** Completed segments in the scratch ring. */
  segments: number;
  /** Bytes the ring holds on disk. */
  bytes: number;
  /** Media time on disk, in ms: how much footage a clip can be cut from. */
  span_ms: number;
  /** Encoder frames dropped because its queue was full. */
  dropped: number;
  /** The same for audio blocks. */
  dropped_audio: number;
  /** Frames the capture source offered that were skipped without being read back. */
  skipped: number;
  /** Achieved frame rate over the last second; 0 until one has been measured. */
  fps: number;
  /** What `encode.fps` asked for. */
  configured_fps: number;
  /** Wall clock minus media time, ms. Positive means media time is behind real time. */
  drift_ms: number;
  /** Clips written this session. */
  clips: number;
  /** Why the engine stopped, when it stopped for a failure. */
  error: string | null;
}

/** What a clip trigger produced, as `clip_now` returns it. */
export interface RecordedClip {
  /**
   * The `clips` row, or `null` when the file was written but the index write failed —
   * the clip exists on disk and will not appear in the list, and the UI has to say so.
   */
  id: number | null;
  path: string;
  duration_ms: number;
  size_bytes: number;
  codec: string;
  /** The clip's first frame on the engine's media timeline. */
  started_at_ms: number;
}

/**
 * What the shell can say about the clip hotkey, as `app_status` returns it.
 *
 * `installed: false` is the interesting case: the chord is still named (it is what the user
 * was told to press) and `error` says why nothing is listening — the chord is taken by
 * another application or another localplay, or this build has no global hotkey at all.
 */
export interface HotkeyStatus {
  /** The configured chord, normalised by the Rust side's `Hotkey` display (`Ctrl+F8`). */
  chord: string;
  /** True only when a listener is really installed in the running process. */
  installed: boolean;
  /** Why it is not installed; `null` exactly when `installed` is true. */
  error: string | null;
}

/**
 * The half of the shell that is not a window, as `app_status` returns it.
 *
 * This is the "the config is a file" contract: the window names the file this process read
 * rather than offering a settings panel it does not have.
 */
export interface AppStatus {
  hotkey: HotkeyStatus;
  /** The `config.toml` this process read, `<app data>/config.toml`. */
  config_path: string;
  /** False when that file does not exist and the example's values are in force. */
  config_exists: boolean;
  /** What closing the window does, in the words the panel shows. */
  close_hint: string;
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
  | 'io'
  | 'recording';

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
