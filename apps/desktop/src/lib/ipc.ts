/**
 * The one place the frontend talks to the Rust side.
 *
 * Two things are worth knowing about this file:
 *
 * 1. **Arrow arguments are snake_case.** The commands are declared
 *    `#[tauri::command(rename_all = "snake_case")]`, so `trim_clip`'s parameters arrive as
 *    `start_ms` / `end_ms` — not the camelCase Tauri would use by default. A mismatch here
 *    is a runtime error ("missing required key start_ms") rather than a compile error on
 *    either side, which is why `ipc.test.ts` asserts the exact key names against a mocked
 *    `invoke`.
 * 2. **Playback goes through the asset protocol, not through a command.** `convertFileSrc`
 *    turns an absolute path into the `asset:` URL that `<video>` can play, and Tauri's
 *    protocol handler checks it against the scope configured in `tauri.conf.json` (and
 *    extended at runtime in `src/lib.rs`). There is no file server in this application,
 *    and no command returns file contents.
 */

import { convertFileSrc, invoke } from '@tauri-apps/api/core';
import type {
  AppStatus,
  ClipDto,
  CommandError,
  DeleteOutcome,
  DeleteSessionOutcome,
  ErrorCode,
  RecordedClip,
  RecordingStatus,
  SessionDto,
  SessionEvent,
  StorageStats,
  ThumbnailRef,
} from './types';

/** The commands this window can call, as an interface. */
export interface ClipSource {
  listClips(): Promise<ClipDto[]>;
  storageStats(): Promise<StorageStats>;
  setFavourite(id: number, favourite: boolean): Promise<ClipDto>;
  trimClip(id: number, startMs: number, endMs: number): Promise<ClipDto>;
  thumbnail(id: number, atMs: number): Promise<ThumbnailRef>;
  deleteClip(id: number): Promise<DeleteOutcome>;
  /**
   * Sessions: one recording, the file it was concatenated into, and the markers on its
   * timeline. Read-only apart from the favourite flag and deletion — the engine that writes
   * a session is the recorder, not this window.
   */
  listSessions(): Promise<SessionDto[]>;
  sessionDetail(sessionId: number): Promise<SessionDto>;
  /**
   * A session's markers, in media-time order. `offset_ms` is media time from the session's
   * start, computed by the store; nothing here recomputes it against the wall clock.
   * An empty list is a real answer — a session nobody tagged has no markers.
   */
  sessionEvents(sessionId: number): Promise<SessionEvent[]>;
  setSessionFavourite(sessionId: number, favourite: boolean): Promise<SessionDto>;
  /** Delete a session: its row, then its segments and its recorded file. */
  deleteSession(sessionId: number): Promise<DeleteSessionOutcome>;
  /** Cut a clip out of a **finished** session, losslessly. Refused while it is recording. */
  extractClip(sessionId: number, startMs: number, endMs: number): Promise<ClipDto>;
  /**
   * Recording, through the same engine the CLI drives (`localplay-recorder`). These four
   * are the only commands that make the shell capture anything, and none of them is
   * called by this window on its own initiative: the user presses the button.
   */
  startRecording(): Promise<RecordingStatus>;
  stopRecording(): Promise<RecordingStatus>;
  recordingStatus(): Promise<RecordingStatus>;
  clipNow(): Promise<RecordedClip>;
  /**
   * The half of the shell that is not a window: the clip hotkey (the chord, and whether a
   * listener is really installed), where `config.toml` was read from, and what closing the
   * window does. Read once when the window opens — none of it changes while it runs.
   */
  appStatus(): Promise<AppStatus>;
  /**
   * Write a frontend error into the application's log, beside the backend's own.
   *
   * It is on this surface rather than called through `invoke` directly because the suite below
   * cross-checks these methods against the handler list in `lib.rs`: a command reached one way on
   * one side and another way on the other is exactly what that guard exists to catch.
   */
  logFromFrontend(level: 'error' | 'warn' | 'info', message: string, detail: string): Promise<void>;
  /** An `asset:` URL for an absolute path, for `<video>` and `<img>`. */
  assetUrl(path: string): string;
}

/** The real IPC, backed by the Tauri runtime. */
export const tauriIpc: ClipSource = {
  listClips: () => invoke<ClipDto[]>('list_clips'),
  storageStats: () => invoke<StorageStats>('storage_stats'),
  setFavourite: (id, favourite) => invoke<ClipDto>('set_favourite', { id, favourite }),
  trimClip: (id, startMs, endMs) =>
    invoke<ClipDto>('trim_clip', { id, start_ms: startMs, end_ms: endMs }),
  thumbnail: (id, atMs) => invoke<ThumbnailRef>('thumbnail', { id, at_ms: atMs }),
  deleteClip: (id) => invoke<DeleteOutcome>('delete_clip', { id }),
  listSessions: () => invoke<SessionDto[]>('list_sessions'),
  sessionDetail: (sessionId) => invoke<SessionDto>('session_detail', { session_id: sessionId }),
  sessionEvents: (sessionId) =>
    invoke<SessionEvent[]>('session_events', { session_id: sessionId }),
  setSessionFavourite: (sessionId, favourite) =>
    invoke<SessionDto>('set_session_favourite', { session_id: sessionId, favourite }),
  deleteSession: (sessionId) =>
    invoke<DeleteSessionOutcome>('delete_session', { session_id: sessionId }),
  extractClip: (sessionId, startMs, endMs) =>
    invoke<ClipDto>('extract_clip', { session_id: sessionId, start_ms: startMs, end_ms: endMs }),
  startRecording: () => invoke<RecordingStatus>('start_recording'),
  stopRecording: () => invoke<RecordingStatus>('stop_recording'),
  recordingStatus: () => invoke<RecordingStatus>('recording_status'),
  clipNow: () => invoke<RecordedClip>('clip_now'),
  appStatus: () => invoke<AppStatus>('app_status'),
  logFromFrontend: (level, message, detail) =>
    invoke<void>('log_from_frontend', { level, message, detail }),
  assetUrl: (path) => convertFileSrc(path),
};

/**
 * Whether a rejection is one of our structured command errors.
 *
 * Tauri hands the serialised error straight back to the caller, so the value reaching a
 * `catch` is a plain object with `code` and `message` — not an `Error` instance, and not
 * carrying a stack.
 */
export function isCommandError(err: unknown): err is CommandError {
  if (typeof err !== 'object' || err === null) return false;
  const candidate = err as { code?: unknown; message?: unknown };
  return typeof candidate.code === 'string' && typeof candidate.message === 'string';
}

/** The error class, or `null` if this is not one of ours. */
export function errorCode(err: unknown): ErrorCode | null {
  return isCommandError(err) ? err.code : null;
}

/**
 * A message worth putting in front of the user.
 *
 * Tauri rejections arrive in three shapes — our `CommandError`, a string, or a plain
 * `Error` from the runtime itself (a command that is not registered rejects with a string
 * like "Command not found") — and none of them should render as "[object Object]".
 */
export function errorMessage(err: unknown): string {
  if (isCommandError(err)) return err.message;
  if (typeof err === 'string') return err;
  if (err instanceof Error) return err.message;
  if (err === null || err === undefined) return 'The command failed for an unknown reason.';
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}
