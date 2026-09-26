# Handoff — the desktop app's diagnosability, and what its log found

Written 2026-09-26 because the session running this work kept dying to `model-error` on the
agent's side (never the box, never the build). Everything below is committed and pushed; the
repository is `better-outplayed`, public, and `main` is clean.

## The state in one paragraph

The app had no usable diagnostics: `init_tracing` wrote to **stderr**, which Windows discards for a
windowed process, so every `warn!`/`error!` the backend emitted went nowhere and the only way to
report a fault was a screenshot. That is now fixed, the app was rebuilt and run on the box, and the
log it produced **named the root cause of all four reported symptoms** — a stale clip index. That
is fixed too. What remains is verifying the fix on the box, two tests, and the interface pass.

## Done, with commits

| commit | what it did |
|---|---|
| `d190944` | `sidecar_command()` — every subprocess gets `CREATE_NO_WINDOW`, so no console windows |
| `158fc5a` | links `d190944` correctly and gives the app a **log file** + a **panic hook** |
| `8902a6e` | the **frontend's** errors go to the same log (`log_from_frontend`, `src/lib/diagnostics.ts`) |
| `aeb6c5c` | **`list_clips` omits clips whose file is gone; `thumbnail` refuses one before spawning ffmpeg** |

The log is at `%LOCALAPPDATA%\localplay\logs\better-outplayed.log` on the box, appended, with
`RUST_LOG` overriding the level (default `info,localplay_desktop=debug`).

## The root cause, from the app's own log

```
INFO  clip index ...\localplay\localplay.db (schema v4): 13 clips, 1325281741 bytes
ERROR tauri::protocol::asset: File does not exist at path: ...\clips\clip-1790359133.mp4
```

Thirteen clips and 1.3 GB indexed whose files were not on disk. The window asked the asset protocol
for each one (Tauri's error above), then asked for a **thumbnail** of each — and every thumbnail is
an **ffmpeg process**. Thirteen dead clips meant a burst of ffmpeg children: **terminal windows
appearing and vanishing, the machine nearly falling over, and a main thread too busy to drag its own
window.** One cause, all four symptoms. The window config was never at fault (`resizable: true`,
no `decorations: false`).

The rows came from a cleanup that deleted files without their entries. But it is a bug regardless:
the index is a cache and the filesystem is the truth. `list_clips` now skips missing files (skipped,
**not** deleted — a file can be temporarily unavailable), and `thumbnail` refuses one before it
reaches for the binaries. Because the *listing* is what stops the UI asking, the stale rows already
on the box became harmless — **no database surgery was needed**.

## What is left

1. ~~**Verify the fix on the box**~~ — **done, and it holds.** With the box awake, the log from the
   rebuilt app reads exactly as predicted:

   ```
   INFO  clip index ...: 13 clips, 1325281741 bytes
   DEBUG skipping clip #13 ... is gone          (x12)
   WARN  12 of 13 indexed clips are missing from disk and were left out of the listing
   ```

   **No `ERROR tauri::protocol::asset` lines at all**, where the previous run had one per dead clip.
   With the listing filtered the window never asks for those assets or thumbnails, so no ffmpeg is
   spawned and the storm cannot happen. The app ran (pid 24728, 28 MB, down from 37 MB).
2. **Two tests** — the debugging skill wants a failing test *before* a fix and both fixes were
   written first. `commands.rs`'s own test module is the home: it drives real command bodies against
   a real SQLite store (see its module docs).
3. **The interface pass** — not started. Load the **`impeccable`** skill; `App.svelte` (472 lines)
   and `app.css` (178) plus `apps/desktop/src/lib/` are the surface. The complaint was clunky and
   cluttered.
4. **Confirm the console windows are gone** — the log cannot show a window. With no ffmpeg spawned
   for dead clips the storm is gone by construction, but the honest check is a human watching.

## Working on the box (Flaviowin, `192.168.0.181`, user `flavio`)

* **The shell is PowerShell, and inline SSH PowerShell gets mangled** — `$var`, `$_`, `;`, even
  quoted paths. Write a `.ps1` locally, `scp` **one file at a time** (multi-file scp silently
  transfers nothing), then run
  `powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\Flavio\<name>.ps1`.
* **Session 0 has no desktop.** A GUI run needs `schtasks /Create ... /IT`; builds and probes do not.
  A detached task is how anything long survives an SSH drop.
* **The build**: node is a *portable zip* at `C:\Users\Flavio\localplay-ram\node` (not installed),
  the sidecars are fetched into `localplay-ram\binaries`, and the bundle builds from
  `apps/desktop` with `.\node_modules\.bin\tauri build --bundles nsis`. **`npm run tauri -- build`
  loses its flags under PowerShell** — PowerShell strips `--`; see `docs/packaging.md` §3.1.
* **Read the log, don't ask for a screenshot**:
  `Get-Content $env:LOCALAPPDATA\localplay\logs\better-outplayed.log -Tail 40`
* **Clean up when finished**: scheduled tasks (`bo-app`), the scripts in `C:\Users\Flavio`, and
  `localplay-ram` (portable node, ~210 MB of sidecars, a few GB of Rust target).

## Safety, non-negotiable

Flaviowin runs **Vanguard**. Never synthesise input, never POST/PUT to the LCU, no injection, and
**no screen capture while a protected game is running** — check for `League of Legends`,
`LeagueClient`, `LeagueClientUx` before any run that touches the display. `cargo build`, `cargo
test` and the probe are safe (CPU only).

## Environment notes that cost time

* The **desktop is a separate cargo workspace** at `apps/desktop/src-tauri`; its tests must run from
  there (112 of them).
* `CARGO_HOME="$PWD/target/cargo-home"` on macOS, or cargo cannot write its home.
* The Windows cross-check covers `media`, `replay`, `encoder`, `capture`, `events` — **not** the
  desktop, whose lockfile needs the MSVC toolchain.
* Suites: 546 Rust workspace, 112 desktop shell, 134 vitest.
* **This project's CI went green for the first time** when the repo became public; a red run is now
  a real signal.
