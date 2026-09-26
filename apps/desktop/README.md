# better-outplayed — desktop shell (Phase 2)

The session review window: clip list, player with a timeline scrubber, lossless trim, and a
storage panel. Tauri v2 shell + Svelte 5 frontend, per [spec §3](../../docs/specs/2026-09-23-localplay-design.md)
and [spec §9](../../docs/specs/2026-09-23-localplay-design.md).

The window is **dark-mode only** and has **no network access**: it reads the same SQLite
index the recorder writes, plays clips through Tauri's asset protocol, and shells out to
ffmpeg for trims and thumbnails. Nothing else.

## Layout

```
apps/desktop/
├── package.json, vite.config.ts, tsconfig.json, svelte.config.js, index.html
├── scripts/
│   ├── shots.mjs             # headless render + screenshot + assertion run (`npm run shots`)
│   ├── png-stats.mjs         # the minimal PNG decoder that proves a shot is not blank
│   └── require-sidecars.mjs  # `beforeBundleCommand`: refuse to bundle without the sidecars
├── src/                      # Svelte 5 + TypeScript
│   ├── App.svelte            # the shell: list / detail / storage panel, all state
│   ├── app.css               # the dark palette and the two-pane layout
│   └── lib/
│       ├── types.ts          # the DTOs, mirrored from src-tauri/src/commands.rs
│       ├── ipc.ts            # every `invoke` in the application
│       ├── time.ts           # duration/byte/date formatting
│       ├── recording.ts      # recorder + hotkey readout rules
│       ├── trim.ts           # trim-range clamping, validation, timeline geometry
│       ├── clips.ts          # clip-list view model
│       ├── markers.ts        # a session's timeline markers: colours, placement, click-lead
│       ├── sessions.ts       # session view model, duration labels, extraction blockers
│       └── components/       # ClipList, ClipDetail, SessionList, SessionDetail,
│                             # RecordingPanel, Timeline, StoragePanel
└── src-tauri/
    ├── Cargo.toml            # its own workspace — see below
    ├── build.rs, capabilities/default.json
    ├── tauri.conf.json       # product metadata, the sidecar resource map, app/bundle config
    ├── tauri.windows.conf.json, tauri.macos.conf.json   # bundle targets per platform
    ├── icons/                # generated icon set, `app-icon.png` (the placeholder source),
    │                         # and the three tray icons (`tray-{idle,recording,attention}.png`)
    └── src/
        ├── main.rs           # the entry point
        ├── lib.rs            # state + the thin `#[tauri::command]` wrappers + the Tauri half
        │                     #   of the background wiring (tray, hotkey thread, close-to-hide)
        ├── background.rs     # the tray menu, the hotkey status, the close rule, autostart —
        │                     #   all of it plain functions, plus their tests
        ├── commands.rs       # everything the commands actually do, plus its tests
        └── config.rs         # the desktop half of config.toml: the `[storage]` /
                              #   `[hotkeys]` / `[app]` reader, the settings panel's
                              #   `[storage]` / `[encode]` / `[mic]` / `[games]` reader,
                              #   and the comment-preserving writer
```

## The IPC commands

Each one is a plain function in `commands.rs` over explicit dependencies (a `Store`, an
optional `FfmpegBinaries`, paths, a storage policy). The `#[tauri::command]` wrapper in
`lib.rs` resolves the application state and delegates; it decides nothing. That is what lets
the tests drive the real command bodies against a real SQLite file and real ffmpeg output
with **no Tauri runtime and no window**.

| command | delegates to | notes |
|---|---|---|
| `list_clips()` | `Store::list_clips` | Re-ordered by `created_at` (wall clock), not the store's `started_at` (media time, not comparable across captures). Newest first, id breaks ties. **Rows whose file is gone are left out** — the index is a cache and the filesystem is the truth; the rows stay, so a temporarily unavailable file comes back. |
| `storage_stats()` | `Store`, `plan_cleanup` | Usage, the cap and age limit, and the policy's verdict — including whether the favourites alone exceed the cap. Counted over the clips **on disk**, from the same function `list_clips` uses, with `missing_count` naming the rows that leaves out. |
| `set_favourite(id, favourite)` | `Store::set_favourite` | Rejects an id with no row rather than reporting a no-op as success. |
| `trim_clip(id, start_ms, end_ms)` | `localplay_media::edit::trim_lossless` | A **stream copy** (`-c copy`), never a re-encode. Writes `<name>.trim-<start>-<end>.<ext>` beside the original, probes the result and indexes it. |
| `thumbnail(id, at_ms)` | `localplay_media::edit::thumbnail` | One JPEG in the thumbnails cache, reused on the next request. |
| `delete_clip(id)` | `Store::delete_clip_returning_path` | Row first, then the file (spec §8.2). Reports an orphan rather than hiding a failed unlink. |

| `start_recording`, `stop_recording`, `recording_status`, `clip_now` | `RecorderHost` | The recording engine the CLI also drives. `clip_now` waits for the post-roll, so it — like a start — runs off the webview thread. |
| `get_settings()` | `config::read_effective_settings` | The settings panel's values, with per-key provenance for the ones that fell back to the example. Applies **the engine's rules and nothing more**: a value the recorder loads has to render, or the panel that repairs the file cannot open. |
| `update_settings(update)` | `config::write_settings` | Every field optional. Validates, proves the new clips directory usable **before** writing it, then returns `applied` / `restart_required`. The panel's own stricter policy (the fps band, the clips-dir hygiene) lives here; engine keys take effect on the next start. |
| `app_status()` | `AppState` | The clip hotkey (the chord, and whether a listener is really installed), where `config.toml` was read from, and the sentence that explains closing the window. Read once; none of it changes while the process runs. |

Failures are a serialisable `CommandError { code, message }` with a machine-readable code
(`clip_not_found`, `invalid_range`, `out_of_range`, `invalid_input`, `ffmpeg_unavailable`,
`store`, `media`, `io`). Nothing panics, and `trim_clip` refuses an empty or inverted range
before ffmpeg is ever spawned.

## Where the files live

The shell resolves the application data directory by the **same rule as the CLI**
(`%LOCALAPPDATA%\localplay` on Windows) — deliberately, because the two processes have to
agree on which index and which clips directory they are working with:

| path | what |
|---|---|
| `<app data>/localplay.db` | the clip index (spec §5.5), written by the recorder, read here |
| `<app data>/clips/` | clips; a trim's new file lands beside its parent |
| `<app data>/thumbnails/` | the thumbnail cache, created on first use |
| `<app data>/config.toml` | read by the recorder and by the settings panel, and **written** by the panel: `[storage]`, `[encode]`, `[mic]`, `[games]`. Values fall back per key to `config.example.toml`, and the panel says which ones did. |

## Playback and the asset protocol

Clips play through Tauri's asset protocol, not through a command and not through a file
server. `tauri.conf.json` enables it and scopes it statically to
`$LOCALDATA/localplay/{clips,thumbnails}/**`; `lib.rs` additionally scopes the directories
the process actually resolved at startup, which is what makes a configured
`storage.clips_dir` outside the application data directory playable at all. Range requests
come from the protocol handler, which is what lets the `<video>` element's own scrubbing
work.

## Running it

```sh
cd apps/desktop
npm install
npm run build          # vite → dist/, which tauri.conf.json's frontendDist points at
npx @tauri-apps/cli@^2 dev      # cargo-tauri is not installed globally
```

`cargo-tauri` is deliberately not assumed: `@tauri-apps/cli` is a devDependency here, so
`npx` resolves the pinned version.

### Looking at it without opening a window

```sh
cd apps/desktop
npm run shots          # build, serve dist/, screenshot it headlessly into target/
```

`npm run shots` builds the frontend, serves `dist/` from a throwaway static server, loads it
in headless Chromium with the Tauri IPC replaced by a mock, drives the real UI with real
pointer events and writes PNGs plus a per-shot blank-detection table to
`<repo>/target/desktop-shots/` (gitignored). It fails on a blank, transparent, white or
error-page screenshot, on a contrast ratio below WCAG AA, and on any console, page or HTTP
error it cannot explain.

The mock reaches the window through the test-only seam in `src/main.ts`: the page must be
loaded with `?test-clip-source=1` *and* have set `globalThis.__localplayClipSource`, so a
shipped window — loaded from `tauri://localhost/` — cannot be armed at all. Headless
Chromium is downloaded into `target/desktop-shots/browsers/` on first use
(`npm run shots:browser` does just that step). Media for the fixtures is generated with the
ffmpeg on `PATH`; without ffmpeg the run still produces screenshots and says so.

### This crate is not a workspace member

`apps/desktop/src-tauri` is its own workspace (and is `exclude`d from the root one).
`tauri::generate_context!` reads `tauri.conf.json` at compile time and fails when the
frontend it names (`apps/desktop/dist`) does not exist, so membership would make every
`cargo test --workspace` depend on `npm run build` having run first. Run its tests from
`apps/desktop/src-tauri`.

## Background: tray, hotkey, close-to-hide

The window is a review pane. The application is a **tray application that can record with no
window at all**, and that is the half this directory's `background.rs` owns:

| Behaviour | Where the decision lives | Tested? |
|---|---|---|
| Close the window → hide it (never quit, never end a recording); quit only from the tray | `background::close_action` | Yes, in `background.rs` — the rule is a function so a test can pin it |
| `Ctrl+F8` takes a clip, through `RecorderHost::clip_now` (the same call the Save clip button makes) | `background::install_hotkey` + `on_hotkey_press` | Yes: one press → exactly one clip, driven against a real recording (`commands.rs`) |
| A chord that cannot be registered is reported, not swallowed | `background::install_hotkey`, and `localplay_events::hotkey` returning the `RegisterHotKey` error | Yes (the parse and Windows-only branches); the registration itself needs Windows |
| The tray icon, tooltip and dynamic menu labels | `TrayView::{tray_state, tooltip, rendering}` and `MenuAction::{label, enabled}` | Yes, as pure values |
| Menu item → the calls it makes (including stop-before-quit) | `background::dispatch`, over the `Shell` trait | Yes, against a recording fake |
| `[app] start_with_system` → a Run-key entry | `background::sync_autostart`, `reg_argv`, `registered_path` | The decision table and the `reg.exe` argv: yes. The registry write: **no — Windows only** |

Two things it deliberately does *not* do. It does not add a second trigger: there is one
`clip_now`, and the button, the tray item, the hotkey and the CLI all reach it. And it does
not add a settings panel: the chord is `[hotkeys] clip`, autostart is `[app]
start_with_system`, and the window prints *which file* it read so the file can be found (the
tray's "Open config file" opens it). What is **not** verified is what needs a desktop: that
the tray appears, that the icon changes, that the X hides the window and that a real
keypress arrives. See [`../../docs/verification-status.md`](../../docs/verification-status.md).

Tauri features enabled for this, and why: `tray-icon` (there is no `tauri::tray` without it)
and `image-png` (so the committed `icons/tray-*.png` can be decoded by `Image::from_bytes`).
No plugin and no capability were added — the tray, the window and the autostart entry are all
Rust-side, and the webview is granted nothing new.

## Deliberately not here

- **No capture, no window enumeration, no input synthesis.** The capture crates exist and
  the recorder drives them; this shell never touches WGC or WASAPI.
- **No re-encode path.** The only export is `trim_lossless`.
- **No game-event markers on the timeline.** Spec §9 describes markers rendered from the
  `events` table, and the table exists — but nothing writes to it, because the game-event
  integrations are Phase 4. The timeline draws clips and nothing else rather than markers
  for data that does not exist.
- **No cleanup action in the UI.** The cleanup pass belongs to the recorder, which runs it
  at startup and on a timer. The storage panel reports the verdict and says who acts on it.
- **No CSP yet.** `app.security.csp` is `null`, which is the framework's own default. The
  asset protocol injects its own sources into a configured CSP, and picking one without
  being able to observe a webview would risk silently blocking playback — so it is left for
  a pass that can.
- **No installer that anyone can run yet.** Bundling *is* configured — `bundle.active` is
  `true`, `bundle.resources` embeds the ffmpeg sidecars where `localplay-media` looks for
  them, and a macOS `.app` was built and inspected — but the artifacts are unsigned, the icon
  is a placeholder, there is no updater, and nothing has been installed or uninstalled on
  Windows. Read [`../../docs/packaging.md`](../../docs/packaging.md) before treating any of
  this as shippable.

## Verification status

Run and passing:

```sh
cd apps/desktop && npm test          # 108 vitest tests
cd apps/desktop && npm run build     # vite production build
cd apps/desktop && npm run check     # svelte-check: 0 errors, 0 warnings
cd apps/desktop && npm run shots     # 14 headless screenshots, 201 assertions, 0 failures
cd apps/desktop/src-tauri && cargo test    # 103 tests, against a real store and real ffmpeg
cd apps/desktop/src-tauri && cargo clippy  # no warnings in shipping code
```

**Seen, headlessly:** the frontend rendered in Chromium at 1280x800, in ten states, with the
built bundle and a mocked `ClipSource` — the clip list, the detail view, the timeline, a
drag of both trim handles (3240ms → 9331ms, from real pointer input), a scrub that moved both
the playhead and the `<video>`, the trim notice, the recorder panel while it is recording
(the chord to press and the sentence that explains closing the window are in it), and a
state where the hotkey could **not** be registered (`11-hotkey-not-installed`: the alert, the
reason and the second-instance case, with Start recording still offered, because the window
works without a hotkey). Text contrast was measured, not eyeballed (6.5:1 to 16:1 against
their own backgrounds). `npm run shots` regenerates all of it and is the check that fails if
any of it breaks.

**Still never opened:** the Tauri window itself. No display was used and no webview was
launched, so none of the following has been seen working: the shell and OS window chrome,
the configured window size and DPI scaling, WebKit/WebView2 (the screenshots are Chromium),
the asset-protocol scope check and range requests over `asset:` URLs (the screenshots' media
came from a plain static server), real `<video>` playback of a real clip file, and the trim's
ffmpeg stream copy (the screenshots' trim was a mock mutating an array, which is why the clip
it returns plays black).

**And nothing about the background half has been *observed* — only tested.** No tray icon has
been drawn on any taskbar, no left click has opened its menu, no window has been closed to
watch it hide, no `Ctrl+F8` has been pressed, and the `reg.exe` autostart write has never run
(the suite is not allowed to touch a real Run key). What *is* established is narrower and
should be read as exactly that: the decisions are unit-tested, the trigger path writes a real
clip through the real engine, and the Windows-only code compiles for
`x86_64-pc-windows-msvc` (`cargo check --target`, which CI runs for `crates/events` — where
the `RegisterHotKey` call lives). The per-item levels are in
[`../../docs/verification-status.md`](../../docs/verification-status.md).
