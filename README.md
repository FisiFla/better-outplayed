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

**Pre-alpha — repository initialized, PoC not yet landed.** Nothing here is usable
yet. See [Roadmap](#roadmap) for what is actually built versus planned.

The PoC targets Windows only. Development currently happens on a macOS host, which
means the capture/encode path is written blind and must be verified on Windows
hardware before it can be considered working.

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
├── Cargo.toml                  # workspace manifest
├── crates/
│   ├── capture/                # WGC + DXGI backends behind one trait
│   ├── encoder/                # ffmpeg sidecar + NVENC/QSV/AMF selection
│   ├── replay/                 # ring buffer, trigger windows, clip splice
│   ├── media/                  # ffmpeg/ffprobe sidecar driver, lossless trim
│   ├── events/                 # LoL Live Client, CS2/Dota2 GSI, hotkeys
│   └── store/                  # SQLite schema + clip/session index
├── apps/
│   ├── localplay-cli/          # headless PoC binary (Phase 1)
│   └── desktop/
│       ├── src-tauri/          # Tauri v2 app, IPC commands, tray (Phase 2)
│       └── src/                # Svelte frontend (timeline, scrubber, settings)
├── docs/
│   └── specs/                  # design + plan docs, one per phase
├── xtask/                      # build helpers, sidecar fetch/verify
├── config.example.toml
├── LICENSE-MIT
├── LICENSE-APACHE
└── .gitignore
```

The design spec is at
[`docs/specs/2026-09-23-localplay-design.md`](docs/specs/2026-09-23-localplay-design.md).

---

## Prerequisites (target: Windows 10 1903+ / Windows 11)

- **Rust** stable (MSVC toolchain)
- **Node.js** 20+ and npm
- **WebView2 Runtime** (preinstalled on Win11, evergreen on Win10)
- A GPU with a hardware encoder: NVIDIA (NVENC), Intel (QuickSync), or AMD (AMF)
- `ffmpeg`/`ffprobe` — fetched as sidecars by `xtask`, not committed

> **Note:** the current PoC is Windows-only. The repository does not yet build on
> macOS or Linux, and the development host is macOS — so the capture path is
> unverified until it runs on real Windows hardware.

---

## Roadmap

- [ ] **Phase 0** — repo, tooling, design spec *(in progress)*
- [ ] **Phase 1** — capture + loopback audio + hardware encode + ring buffer + hotkey trigger (PoC)
- [ ] **Phase 2** — Tauri shell, timeline scrubber, clip trim/export
- [ ] **Phase 3** — SQLite index, storage manager, auto-cleanup policies
- [ ] **Phase 4** — LoL Live Client + CS2/Dota2 GSI event integrations
- [ ] **Phase 5** — full-session recording, chapter marks, packaging/installer

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
