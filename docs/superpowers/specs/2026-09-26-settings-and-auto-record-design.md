# Settings and auto-record visibility/control — design

Date: 2026-09-26. Status: approved for implementation (option A: persist instantly,
engine keys take effect on next recorder start; no auto-restart).

## 1. Problem

The review window is the app's only surface, and it exposes nothing the recorder runs
with: clip directory, fps, resolution, mic, and `[games] auto_record` all live in
`config.toml`, read at startup, with no reader in the window and no writer anywhere.
Two concrete defects ride along:

- The window's Start (and the tray's) builds `RecorderOptions::default()` — replay
  buffer, no microphone, nothing watched — so a config file's `[mic]` and `[games]`
  sections are silently ignored when recording starts from the desktop.
- The engine tracks `watching_games` and the matched game in `RecorderStatus`, but the
  `RecordingStatusDto` mapping drops them, so the window cannot say whether anything
  is being watched. Default config has `auto_record = false`, i.e. out of the box
  nothing auto-records and no watcher thread exists at all.

## 2. Scope

In: `get_settings` + Settings panel (display); `update_settings` incl. auto-record
toggle (persist + scoped live-apply); watcher/mode/mic status in the recorder panel;
Start plumbing mic/games from config; comment-preserving TOML writer; Rust + vitest +
shots coverage.

Out: watch-list editing (titles display read-only); record mode switching (Start stays
buffer mode, as today); caps editing (YAGNI — not requested); live engine
reconfiguration without restart (engine bakes all of it per start; revisit later);
Windows-NSIS packaging changes (none — no new binaries).

## 3. IPC surface

- `get_settings` → `SettingsDto { clips_dir, clips_dir_resolved, fps, output_size,
  mic_enabled, auto_record, watch_titles: string[], config_exists, config_path }`.
  Built from the same loader Start uses (file, else example fallback). No new
  failure modes beyond the loader's own.
- `update_settings` takes all fields optional; unknown/absent fields are left alone.
  Returns `{ applied: string[], restart_required: bool, restart_reason: string|null }`.
  `applied` echoes the keys that changed; `restart_reason` is null unless
  `restart_required` is true, in which case it names the changed engine keys.
  `restart_required` is true only when the recorder slot holds a running recorder AND
  an engine key (fps, output_size, mic, auto_record) changed.
- `RecordingStatusDto` gains `watching_games: bool`, `matched_game: string|null`,
  `recorder_mode: string`, `mic_enabled: bool` — mapped, not invented (all four exist
  in engine `RecorderStatus` today).
- `ipc.test.ts` handler cross-check and invoke-key assertions extend to both commands
  (existing pattern).

## 4. Backend

- Desktop `config.rs` learns `[mic]` and `[games]` read sections (same file, parsed at
  Start like the capture sections — the shell must still open when they are missing
  or wrong).
- New writer module using `toml_edit` (add `toml_edit = "0.22"` to the desktop
  manifest; `toml` 0.8 already pulls that line). Edits set dotted keys on the parsed
  document so comments and formatting survive. Validation is reused, not duplicated:
  serialize the edited document and run the existing `from_toml` validators on it
  *before* touching disk; a failure returns `InvalidInput` naming the key and writes
  nothing. The file write is atomic (temp + rename); a missing file is created with
  the example as its base so the write changes exactly what was asked.
- Live-apply, key by key:
  - `clips_dir`: immediate. Rebuild `AppState.paths` via `AppPaths::resolve`,
    `create_dir_all`, re-allow the new directory on the asset-protocol scope, store
    the new `StorageConfig`. Existing index rows stay (their files are untouched);
    only new writes go to the new directory. The scope call is a 3-line imperative
    tail (no app handle exists headless); everything around it is pure and tested.
  - `fps`, `output_size`, `mic_enabled`, `auto_record`: persisted only. The engine
    snapshots them per start (`Prepared`), so they take effect on the next Start.
    Changing `auto_record` while idle arms/disarms the *next* recording; while
    recording the response says so via `restart_required`.
- Start plumbing: build `RecorderOptions` from the loaded `[mic]`/`[games]` (mic on,
  watcher with configured titles) instead of `default()`. Mode stays buffer.
- No background timers change; caps are untouched.

## 5. Frontend

- Sidebar `Settings` panel under Storage, three mini-sections each with its own
  Apply button sending that section's fields together: Library (clip dir text
  field + Apply), Capture (fps number, resolution select Native/1920×1080/1280×720/
  960×540 mapping to `""`/WxH, mic toggle), Automatic (auto_record toggle, watched
  titles, status line). The notice banner confirms what applied, and engine keys show
  "takes effect on next recording" while running.
  Backend validation failures surface through the existing error banner.
- Recorder panel gains one auto-record line: off → "Automatic recording is off";
  armed → "Watching for {titles}"; matched → "{game} detected" (from status).
- `types.ts`/`clips.ts`-style pure helpers only where a screenshot cannot check the
  rule (e.g. the restart-required wording); presentation stays in components.
- Shots: mock gains `getSettings`/`updateSettings` (+ settings fixture data) and the
  extended `recordingStatus`; new `14-settings` state asserts fields, toggle, and the
  next-start hint; existing states unchanged unless the panel moves their pixels
  (layout audits will say).

## 6. Error handling

- Malformed values: `InvalidInput` naming the key, nothing written, banner shows it.
- Missing config file on update: created from the example, then edited (same guarantee).
- Recorder running + engine key changed: success + `restart_required`, never a silent
  partial apply and never an implicit restart (option A).
- clips_dir uncreatable/unwritable: `Io` naming the path, `AppState` untouched.

## 7. Tests

- Rust (desktop shell): writer round-trip (comments/format preserved, only targeted
  keys change), validation rejections write nothing, `restart_required` matrix
  (idle/running × storage-only/engine keys), DTO field mapping, options plumbing
  (mic/games from config reach `RecorderOptions`), clips_dir re-resolve unit.
- vitest: IPC key names + handler-list guard for both commands.
- Shots: `14-settings` state + auto-record assertions in recorder states; green
  across svelte-check, vitest, shots and the desktop shell Rust suite locally, with
  the full workspace suite left to CI.
- Not verifiable headless: the asset-scope re-allow (code-reviewed, 3 lines), real
  game matching (needs Windows + a game — user acceptance), mic hardware path.

## 8. Rollout

Push to main (CI covers workspace + frontend + bundle). The user runs the installed
app from a box build: a fresh Flaviowin build is needed before any of this is visible
there (the build tree was cleaned up after the last session).
