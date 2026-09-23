# localplay — desktop shell (Phase 2)

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
├── src/                      # Svelte 5 + TypeScript
│   ├── App.svelte            # the shell: list / detail / storage panel, all state
│   ├── app.css               # the dark palette and the two-pane layout
│   └── lib/
│       ├── types.ts          # the DTOs, mirrored from src-tauri/src/commands.rs
│       ├── ipc.ts            # every `invoke` in the application
│       ├── time.ts           # duration/byte/date formatting
│       ├── trim.ts           # trim-range clamping, validation, timeline geometry
│       ├── clips.ts          # clip-list view model
│       └── components/       # ClipList, ClipDetail, Timeline, StoragePanel
└── src-tauri/
    ├── Cargo.toml            # its own workspace — see below
    ├── build.rs, tauri.conf.json, capabilities/default.json, icons/icon.png
    └── src/
        ├── main.rs           # the entry point
        ├── lib.rs            # state + the thin `#[tauri::command]` wrappers
        ├── commands.rs       # everything the commands actually do, plus its tests
        └── config.rs         # the `[storage]` half of config.toml
```

## The IPC commands

Each one is a plain function in `commands.rs` over explicit dependencies (a `Store`, an
optional `FfmpegBinaries`, paths, a storage policy). The `#[tauri::command]` wrapper in
`lib.rs` resolves the application state and delegates; it decides nothing. That is what lets
the tests drive the real command bodies against a real SQLite file and real ffmpeg output
with **no Tauri runtime and no window**.

| command | delegates to | notes |
|---|---|---|
| `list_clips()` | `Store::list_clips` | Re-ordered by `created_at` (wall clock), not the store's `started_at` (media time, not comparable across captures). Newest first, id breaks ties. |
| `storage_stats()` | `Store`, `plan_cleanup` | Usage, the cap and age limit, and the policy's verdict — including whether the favourites alone exceed the cap. |
| `set_favourite(id, favourite)` | `Store::set_favourite` | Rejects an id with no row rather than reporting a no-op as success. |
| `trim_clip(id, start_ms, end_ms)` | `localplay_media::edit::trim_lossless` | A **stream copy** (`-c copy`), never a re-encode. Writes `<name>.trim-<start>-<end>.<ext>` beside the original, probes the result and indexes it. |
| `thumbnail(id, at_ms)` | `localplay_media::edit::thumbnail` | One JPEG in the thumbnails cache, reused on the next request. |
| `delete_clip(id)` | `Store::delete_clip_returning_path` | Row first, then the file (spec §8.2). Reports an orphan rather than hiding a failed unlink. |

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
| `<app data>/config.toml` | `[storage]` only; the values fall back to `config.example.toml` |

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

### This crate is not a workspace member

`apps/desktop/src-tauri` is its own workspace (and is `exclude`d from the root one).
`tauri::generate_context!` reads `tauri.conf.json` at compile time and fails when the
frontend it names (`apps/desktop/dist`) does not exist, so membership would make every
`cargo test --workspace` depend on `npm run build` having run first. Run its tests from
`apps/desktop/src-tauri`.

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
- **No installer.** `bundle.active` is `false`: packaging is Phase 5.

## Verification status

Run and passing:

```sh
cd apps/desktop && npm test          # 86 vitest tests
cd apps/desktop && npm run build     # vite production build
cd apps/desktop && npm run check     # svelte-check: 0 errors, 0 warnings
cd apps/desktop/src-tauri && cargo test    # 43 tests, against a real store and real ffmpeg
cd apps/desktop/src-tauri && cargo clippy  # no warnings outside test bodies
```

**Never run, and therefore unverified:** the window itself has never been opened — no
display is assumed on the development host, and none was used. Nothing about layout,
rendering, pointer interaction, `<video>` playback, or the asset-protocol scope check has
been seen working. The JavaScript-level logic behind all of it is unit-tested, and the
Rust command bodies are tested end to end, but the two are only joined by the assertions in
`src/lib/ipc.test.ts` — which pin the command names and argument keys against the Rust
source — and by the build.
