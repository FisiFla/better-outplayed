# localplay

> **100% local, zero-cloud game clipping and replay buffer for Windows.**

localplay is an open-source alternative to [Outplayed](https://overwolf.com) and
[Ascent](https://tryascent.gg) that runs entirely on your machine. No account, no
telemetry, no cloud upload, no Overwolf runtime. Just a native desktop app that keeps
the last 30–120 seconds of your gameplay in a ring buffer and writes a lossless clip
to disk the instant you press a hotkey or your game reports an event.

---

## Non-negotiable principles

These constrain every design decision in this repo. A change that violates one of
these is a bug, not a tradeoff.

| # | Principle | What it rules out |
|---|-----------|-------------------|
| 1 | **Zero cloud, zero logins** | Accounts, telemetry, crash reporting, cloud upload, SaaS dependencies, feature gates |
| 2 | **No Overwolf** | Any dependency on the Overwolf runtime, its SDK, or its ad-injection model |
| 3 | **Minimal resource usage** | Bundled Chromium/Electron, always-on GPU readback, unbounded RAM buffers |
| 4 | **Auto & manual clipping** | Hotkey-only clipping; game events must be able to trigger the buffer too |
| 5 | **Instant scrubbing & trim** | Re-encoding on export. Trims are FFmpeg stream copies — keyframe-aligned, sub-second |

---

## Status

**Pre-alpha, but far past "initialized".** The replay-buffer pipeline, the storage
manager and the desktop app all exist; Phase 1 has run once on real Windows hardware; the
game-event integrations are implemented. What that run *proved* and *refuted* — and,
critically, the large amount of Windows code that has been written and type-checked but
**never executed** — is recorded item by item in
[`docs/verification-status.md`](docs/verification-status.md). **Read that file before
trusting anything below.**

The short version: on 4K Windows hardware the pipeline runs and produces correct clips,
but it cannot sustain the configured frame rate (**~24 fps against 30 configured, ~45% of
frames dropped**) and costs **~50.8% of a CPU core** against a **< 5%** target. Everything
added since that one session is type-checked for `x86_64-pc-windows-msvc` or tested on
macOS — **not** verified on Windows. The *capture path* targets Windows only; off Windows
the pipeline runs a synthetic stub, so a green `cargo test` proves none of the eight
acceptance criteria.

---

## Architecture

```
                      ┌──────────────────────────────────────────────┐
                      │  localplay.exe  (Tauri v2 shell)             │
                      │  ┌────────────────────────────────────────┐  │
   global hotkey ────▶│  │  frontend (Vite + webview)             │  │
   Ctrl+F8            │  │  dark-mode timeline / scrubber / trim  │  │
                      │  └───────────────┬────────────────────────┘  │
                      │                  │ Tauri IPC (commands)     │
                      │  ┌───────────────▼────────────────────────┐  │
                      │  │  Rust core                             │  │
                      │  │                                        │  │
                      │  │  ┌──────────┐   ┌──────────────────┐   │  │
   game events ──────▶│  │  │ events   │──▶│ replay (ring buf)│   │  │
   (LoL :2999,        │  │  └──────────┘   └────────┬─────────┘   │  │
    CS2/Dota2 GSI)    │  │                          │             │  │
                      │  │  ┌──────────┐   ┌────────▼─────────┐   │  │
                      │  │  │ capture  │──▶│  encoder (HW)    │   │  │
                      │  │  │ WGC/DXGI │   │ NVENC/QSV/AMF    │   │  │
                      │  │  │ + WASAPI │   │ (ffmpeg child)   │   │  │
                      │  │  └──────────┘   └────────┬─────────┘   │  │
                      │  │                          │             │  │
                      │  │  ┌──────────┐   ┌────────▼─────────┐   │  │
                      │  │  │ media    │◀──│  store (SQLite)  │   │  │
                      │  │  │ ffmpeg   │   │  clips / tags    │   │  │
                      │  │  └──────────┘   └──────────────────┘   │  │
                      │  └────────────────────────────────────────┘  │
                      └──────────────────────────────────────────────┘
                                        │
                          ┌─────────────▼──────────────┐
                          │  NVMe scratch + clip store │
                          │  (bounded segment ring)    │
                          └────────────────────────────┘
```

### Key flows

**Replay buffer (the core loop).** `capture` continuously pulls GPU frames via
Windows Graphics Capture and hands them to `encoder`, which emits an
already-compressed H.264/HEVC elementary stream. Because the stream is compressed,
the ring buffer holds *packets*, not raw frames — a 120 s buffer costs tens of MB
rather than several GB. Packets land on a bounded NVMe scratch file and are evicted
oldest-first. On trigger, `replay` splices the persisted window
`[trigger − pre_seconds, trigger + post_seconds]` and hands it to `media` for a
lossless `-c copy` container remux.

**Lossless trim.** Every trim is `ffmpeg -ss <t> -i in -c copy out`. No decode, no
encode, no quality loss. The consequence is honest and intentional: cuts snap to the
nearest keyframe. Clip export works around this by forcing a keyframe on the buffer
boundary; arbitrary scrubbing trims accept keyframe granularity.

**Game events.** Two local-only sources, no network egress:
- *League of Legends* — poll `https://127.0.0.1:2999/liveclientdata/allgamedata`
  (self-signed cert on loopback) for kills/deaths/objectives.
- *CS2 / Dota 2* — a `127.0.0.1`-bound HTTP listener consuming Valve's Game State
  Integration POSTs via a user-installed config file.
- *Everything else* — the global hotkey path, which needs no integration at all.

---

## Tech stack

| Layer | Choice | Why |
|-------|--------|-----|
| Core | **Rust** | Memory safety, no GC pauses, direct Win32/COM bindings, low RSS |
| GUI shell | **Tauri v2** | Uses the OS webview (WebView2) — no bundled Chromium, ~10 MB shell |
| Frontend | **Vite + Svelte** | Small runtime, fine-grained reactivity fits a live timeline well |
| Capture | **Windows Graphics Capture** (fallback: DXGI Desktop Duplication) | Supported on Win10 1903+, per-window capture, no hook injection |
| Audio | **WASAPI loopback** of the default render endpoint | Game audio isn't a capture device; loopback needs no virtual audio driver |
| Encode | **NVENC / QuickSync / AMD AMF** via the bundled `ffmpeg` sidecar | Hardware encoding on the GPU at near-zero CPU cost, and one code path for all three vendors |
| Video I/O | **Bundled `ffmpeg`/`ffprobe` sidecars** | Lossless `-c copy`, thumbnails, remux — no libav build in-tree |
| Index | **SQLite (rusqlite, bundled)** | Single-file, zero-config, embedded; indexes matches/timestamps/tags |

### Rejected alternatives

- **Electron** — 150–300 MB RSS for a UI that shows a timeline. Violates principle 3.
- **Overwolf** — principle 2, and its ad model is the thing we exist to avoid.
- **FFmpeg `gdigrab` for capture** — CPU-bound, no per-window capture, drops frames
  under load. Acceptable as a debug fallback, never as the shipping path.
- **Raw-frame ring buffer** — 1080p60 raw is ~370 MB/s. A 120 s buffer would need
  ~44 GB of RAM. Compressed-packet buffering is the entire trick.

---

## Repository layout

```
localplay/
├── .github/workflows/          # CI: tests + Windows cross-check + frontend (macOS runner)
├── Cargo.toml                  # workspace manifest
├── crates/
│   ├── capture/                # WGC + WASAPI backends (Windows) behind one trait; stub elsewhere
│   ├── encoder/                # ffmpeg sidecar child + NVENC/QSV/AMF selection
│   ├── replay/                 # ring buffer, segment ledger, trigger windows, clip splice
│   ├── recorder/               # the recording engine both front-ends drive
│   ├── media/                  # ffmpeg/ffprobe sidecar driver, lossless trim
│   ├── events/                 # LoL Live Client, CS2/Dota2 GSI
│   └── store/                  # SQLite schema + clip/session/event index, cleanup policy
├── apps/
│   ├── localplay-cli/          # headless binary: a hotkey driver over `recorder`
│   └── desktop/
│       ├── src-tauri/          # Tauri v2 app + IPC command layer over store/recorder
│       └── src/                # Svelte frontend (recorder panel, clip list, player, trim)
├── docs/
│   ├── verification-status.md  # what is verified and how — READ IT FIRST
│   ├── platform-traps.md       # OS behaviours (macOS/BSD vs Linux/Windows) that bite
│   ├── specs/                  # design docs
│   ├── plans/                  # task breakdowns, one per phase
│   └── runbooks/               # manual (Windows) verification procedures
├── xtask/                      # build helpers: `xtask sidecars …`, `xtask probe`, `xtask verify`
├── scripts/                    # PowerShell glue (run the harness in a desktop session)
├── config.example.toml
├── LICENSE-MIT
├── LICENSE-APACHE
└── .gitignore
```

Start with [`docs/verification-status.md`](docs/verification-status.md) — the honest
ledger of what is verified, what is only type-checked, and what is unverified — and
[`docs/platform-traps.md`](docs/platform-traps.md) for the OS behaviours that have already
caused two bugs here. The design spec is at
[`docs/specs/2026-09-23-localplay-design.md`](docs/specs/2026-09-23-localplay-design.md),
the Phase 1 task breakdown at
[`docs/plans/2026-09-23-localplay-phase-1-poc.md`](docs/plans/2026-09-23-localplay-phase-1-poc.md)
and the Phase 4 game-event integrations at
[`docs/plans/2026-09-23-localplay-phase-4-integrations.md`](docs/plans/2026-09-23-localplay-phase-4-integrations.md).

### Game event integrations (Phase 4)

Two sources, both **loopback-only by construction** and both switched on from `[events]` in
the config file:

- **League of Legends** — polls Riot's Live Client Data API
  (`https://127.0.0.1:2999/liveclientdata/allgamedata`) once a second while a game is
  running. The endpoint's certificate is self-signed, so TLS verification is relaxed inside
  one dedicated, address-pinned client; a guard test asserts that no other file in the tree
  relaxes verification or constructs an HTTP client.
- **CS2 / Dota 2** — a `127.0.0.1`-bound HTTP listener for Valve's Game State Integration.
  It requires a generated token (which the game echoes back inside the payload's `auth`
  object), rejects anything that does not carry it, and generates the
  `gamestate_integration_localplay.cfg` you copy into the game's `cfg` directory.

An event takes a clip through **the hotkey's own trigger** — same pre-roll, same post-roll,
same lossless splice in media time — and writes why it was taken into the `events` table,
linked to the clip. A kill, a bomb or the end of a match clips; a round boundary is recorded
as a timeline marker without taking footage. No game has been contacted to verify any of
this yet: the payloads are canned fixtures and the servers are mocks in the test process.

---

## Prerequisites (target: Windows 10 1903+ / Windows 11)

- **Rust** stable (MSVC toolchain)
- **Node.js** 20+ and npm
- **WebView2 Runtime** (preinstalled on Win11, evergreen on Win10)
- A GPU with a hardware encoder: NVIDIA (NVENC), Intel (QuickSync), or AMD (AMF)
- `ffmpeg`/`ffprobe` — either install them on `PATH`, **or fetch the pinned sidecars**
  with `cargo xtask sidecars fetch` (see below). They are not committed.

> **Note:** the *capture path* is Windows-only. The repository builds and its test suite
> runs on macOS and Linux (CI runs it on `macos-latest`); off Windows the pipeline uses a
> synthetic `StubCapture`, so a green test run proves none of the acceptance criteria — see
> [`docs/verification-status.md`](docs/verification-status.md).

### Getting the sidecars

The last item is a command, not a manual download. The URL and the SHA-256 of the archive
it must serve live in `xtask/sidecars.toml`; `fetch` verifies the archive against that hash
and extracts only the two binaries it lists, into `binaries/` (gitignored):

```sh
cargo xtask sidecars record    # download, print and record each archive's sha256 (once)
cargo xtask sidecars fetch     # verify the recorded hash, then extract
# a Windows sidecar can be fetched and inspected from macOS or Linux:
cargo xtask sidecars fetch --target x86_64-pc-windows-msvc
```

`record` writes a **trust-on-first-use** pin: it cannot attest who served the archive, only
the bytes to expect from then on, so the URL and the hash are meant to be reviewed in the
diff and committed. At runtime the app looks, in order, for `binaries/` **next to the
executable** (the Windows installation directory, i.e. where the installer's resource mapping
puts it), then for the **resources of an installed bundle** (`Contents/Resources/binaries`
inside a macOS `.app`), then — for a `cargo run` checkout only — for a `binaries/` directory at
the root of the project the executable lives under, and finally on `PATH`. Installing `ffmpeg`
system-wide is therefore not required, which is the point. The packaging story — what
`tauri build` produces, where the sidecars land, and what a release still needs — is in
[`docs/packaging.md`](docs/packaging.md).

### Verifying a Windows box

The one command that runs the acceptance checks and writes down what it saw:

```powershell
cargo xtask verify
```

It starts the buffer with a bounded, documented configuration (printed in the report) and
takes a clip through the CLI's own in-process `--self-test-clip-after` trigger — **no
keyboard or mouse input is synthesised, sent or injected anywhere in it** — then probes
that clip with ffprobe: duration against the configured window, stream count, codec,
resolution, A/V drift and audio level, next to the process's own CPU and memory against
criterion 6's thresholds, the media-vs-wall-clock ratio, the scratch cap and eviction, and
criterion 7's fail-loud encoder path. Everything lands in `target\verify\report.md`, raw
output inline; checks it cannot perform are marked *not performed* with the reason, and it
exits non-zero only when a check actually failed. Over SSH — where there is no desktop to
capture — use `scripts\verify-in-interactive-session.ps1`, which runs it in the logged-on
user's session through a one-shot scheduled task and cleans the task up afterwards.

The harness **has never run on Windows** — it was built and exercised on macOS against the
synthetic backends — so its own report is part of what a fresh run checks. It cannot verify
the GUI window, a real keypress, anything needing a real game, or the live capture-clock
divergence; §8 of [`docs/verification-status.md`](docs/verification-status.md) lists what
stays manual.

---

## Roadmap

- [x] **Phase 0** — repo, tooling, design spec
- [x] **Phase 1** — capture + loopback audio + hardware encode + ring buffer + hotkey trigger (PoC).
      **Ran on real Windows hardware once** and produced correct clips, but two criteria
      miss: at 4K it sustains ~24 fps against 30 configured (~45% of frames dropped) and
      costs ~50.8% of a CPU core against a < 5% target (open as
      [issue #1](https://github.com/FisiFla/localplay/issues/1)). The fixes written since
      that run are **type-checked, not re-run on Windows**.
- [x] **Phase 2** — Tauri shell, timeline scrubber, clip trim. The window and its
      recording wiring exist; they are exercised **headlessly in CI, never opened as a real
      window on Windows**.
- [x] **Phase 3** — SQLite index, storage manager, auto-cleanup policies *(host-verified)*
- [x] **Phase 4** — LoL Live Client + CS2/Dota2 GSI event integrations *(implemented and
      tested against local mocks; **never run against a real game** — see the
      [Phase 4 note](docs/plans/2026-09-23-localplay-phase-4-integrations.md))*
- [ ] **Phase 5** — full-session recording, chapter marks, packaging/installer *(bundling and
      the sidecar resource mapping are configured and a macOS `.app` was built and inspected;
      **code signing, notarisation, an auto-updater and a Windows install are all still
      missing** — see [`docs/packaging.md`](docs/packaging.md))*

> **Caveat on every `[x]` above.** Only the Phase 1 capture path has ever run on Windows,
> once, and it only partly passed. The Windows-specific code added since that run — and all
> of Phases 2–4 — is type-checked for `x86_64-pc-windows-msvc` or tested on macOS, never
> executed on Windows. The per-item truth, and the ordered list of the next Windows runs
> that would change it, are in [`docs/verification-status.md`](docs/verification-status.md).

Phase 1's eight acceptance criteria and how to run them are in the
[Phase 1 verification runbook](docs/runbooks/phase-1-verification.md).

---

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. This is the Rust ecosystem convention and is compatible with
shipping an LGPL-licensed `ffmpeg` sidecar.

### A note on the `ffmpeg` build

localplay only ever uses **hardware** encoders (`nvenc`/`qsv`/`amf`) and stream copy,
so it never requires `libx264`. That means an **LGPL** `ffmpeg` build is sufficient,
and shipping it does not pull the GPL `libx264`/`libx265` obligations into this
project.
