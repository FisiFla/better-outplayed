# localplay — Design Spec (Phase 1)

- **Status:** Approved
- **Date:** 2026-09-23
- **Phase:** 1 of 6 (see Roadmap)
- **License:** MIT OR Apache-2.0
- **Target platform:** Windows 10 1903+ / Windows 11 (x86_64)
- **Development host:** macOS arm64 — see [Verification constraints](#12-verification-constraints)

---

## 1. Problem & purpose

Outplayed and Ascent solve the "I wish I'd recorded that" problem for gamers, but they
do it through Overwolf, which imposes a heavy runtime, an ad model, and cloud
involvement. localplay exists to deliver the same outcome — a clip of the last N
seconds, available instantly — as a single native binary that never talks to a
network and never asks for an account.

### Non-negotiable principles

These are constraints, not preferences. A change that violates one is a defect.

1. **Zero cloud, zero logins.** No accounts, telemetry, crash reporting, cloud
   upload, or SaaS dependency. The application must be fully functional with the
   network adapter disabled.
2. **No Overwolf.** No dependency on the Overwolf runtime, SDK, or its ad model.
3. **Minimal resource usage.** Must sustain capture and encode while a game is
   running without meaningfully competing for CPU or GPU. This rules out bundled
   Chromium, per-frame GPU readback on the hot path, and unbounded buffers.
4. **Auto and manual clipping.** A global hotkey *and* game-reported events must both
   be able to trigger a clip.
5. **Instant scrubbing and trim.** Export must never re-encode. Trims are FFmpeg
   stream copies.

### Success criteria for Phase 1

Phase 1 is complete when a headless Windows binary can run a replay buffer and write
a correct clip on hotkey. The measurable criteria are in
[§11 PoC scope](#11-poc-scope--success-criteria).

---

## 2. Constraints

| Constraint | Consequence |
|---|---|
| Windows-only capture (WGC/DXGI) | No macOS/Linux build in Phase 1. Non-Windows backends are out of scope. |
| No re-encode on export | Cuts snap to keyframes; see [§6.3](#63-keyframe-snapping-the-central-trade-off). |
| Hardware encoders only | Requires a GPU with NVENC, QuickSync, or AMF. No CPU fallback to `libx264`. |
| Zero network egress | Game integrations may only bind/connect on loopback. |
| No `ffmpeg` in the repo | Sidecars are fetched and checksum-verified at build time. |

---

## 3. Tech stack

| Layer | Choice | Rationale |
|---|---|---|
| Core language | Rust (edition 2021) | Memory safety without GC pauses; direct Win32/COM interop; predictable RSS. |
| Application shell | Tauri v2 | Renders in the OS webview (WebView2). ~10 MB shell versus 150–300 MB for Electron. |
| Frontend | Svelte 5 + Vite + TypeScript | Small runtime; fine-grained reactivity suits a continuously updating timeline. |
| Screen capture | Windows Graphics Capture (WGC) | Win10 1903+; per-window capture; no process injection or API hooking. |
| Capture fallback | DXGI Desktop Duplication | Covers configurations where WGC is unavailable; monitor-granular only. |
| Audio capture | WASAPI loopback (`IAudioClient`) | Game audio is not exposed as a capture device. Loopback of the default render endpoint is the only zero-config route, and it needs no virtual audio driver. |
| Video encode | `ffmpeg` sidecar with `h264_nvenc` / `hevc_qsv` / `h264_amf` | One integration path for all three vendors; ffmpeg is required for trimming regardless. |
| Video I/O | `ffmpeg` / `ffprobe` sidecars (LGPL build) | Lossless `-c copy`, concat, thumbnails. LGPL suffices because only hardware encoders are used. |
| Metadata index | SQLite via `rusqlite` (`bundled` feature) | Single-file, zero-config, no system SQLite dependency. |
| Hotkeys | `RegisterHotKey` (Win32) | OS-level global hotkey; no keyboard hook required. |

### 3.1 Dependency policy

Every crate added to the workspace must be justified against principle 3 (resource
usage) and principle 1 (no network). Crates that phone home, perform background
telemetry, or pull a large transitive tree require an explicit note in the PR.

### 3.2 Rejected alternatives

| Alternative | Rejected because |
|---|---|
| Electron | 150–300 MB baseline RSS for a timeline UI. Violates principle 3. |
| Overwolf | Violates principle 2; its ad model is what localplay exists to avoid. |
| `gdigrab` capture | CPU-bound, no per-window capture, drops frames under load. Debug fallback at best. |
| Raw-frame ring buffer | 1080p60 raw ≈ 370 MB/s; a 120 s buffer would need ≈ 44 GB RAM. |
| Native Media Foundation MFT | Best long-term performance, but heavy COM plumbing. Deferred to Phase 2 behind the `Encoder` trait. |
| Direct vendor SDKs (NVENC/QSV/AMF) | Three separate implementations. Deferred to Phase 2+ behind the same trait. |
| CPU `libx264` fallback | Silently destroys in-game performance. Failing loudly is the correct behavior. |

---

## 4. Repository layout

```
localplay/
├── Cargo.toml                  # [workspace] members + shared lints
├── crates/
│   ├── capture/                # CaptureBackend trait, wgc.rs, dxgi.rs, probe.rs, stub.rs
│   ├── encoder/               # Encoder trait, ffmpeg.rs (sidecar), vendor probe
│   ├── replay/                 # RingBuffer, SegmentLedger, ClipSplicer
│   ├── media/                  # ffmpeg/ffprobe driver, lossless trim, thumbnails
│   ├── events/                 # lol.rs, gsi.rs, hotkey.rs, Trigger
│   └── store/                  # SQLite schema, migrations, clip/session queries
├── apps/
│   ├── localplay-cli/          # headless PoC binary — Phase 1 deliverable
│   └── desktop/                # src-tauri/ + src/ (Svelte) — Phase 2
├── docs/specs/                 # design + plan documents
├── xtask/                      # sidecar fetch/verify, encoder probing
├── config.example.toml
├── LICENSE-MIT
├── LICENSE-APACHE
└── README.md
```

The Rust workspace manifest is authoritative: a crate that is not a workspace member
is a bug.

---

## 5. Component design

Each crate has one purpose, a narrow public surface, and is testable without the
others. Dependencies flow left to right: `capture → encoder → replay → media`, with
`store` and `events` as side inputs to `replay`.

### 5.1 `capture`

Produces frames. Owns nothing downstream.

```rust
pub trait CaptureBackend: Send {
    fn start(&mut self) -> Result<()>;
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>>;
    fn stop(&mut self) -> Result<()>;
    fn capabilities(&self) -> CaptureCapabilities; // resolution, fps, format
}

pub struct Frame {
    pub data: FrameBuffer,       // CPU-visible pixels
    pub pts: Duration,           // monotonic since capture start
    pub format: PixelFormat,     // BGRA8 on the WGC path
}
```

Implementations: `WgcCapture` (primary), `DxgiDuplication` (fallback),
`StubCapture` (non-Windows, generates a synthetic test pattern so the rest of the
workspace type-checks and unit-tests on macOS).

Windows implementations are gated behind `#[cfg(windows)]`. `probe::select_backend()`
prefers WGC and falls back to DXGI, returning a typed error rather than a `null`
result if neither is available.

#### Audio

Audio is captured in the same crate, behind a parallel trait:

```rust
pub trait AudioBackend: Send {
    fn start(&mut self) -> Result<()>;
    fn next_buffer(&mut self, timeout: Duration) -> Result<Option<AudioBuffer>>;
    fn stop(&mut self) -> Result<()>;
}

pub struct AudioBuffer {
    pub data: Vec<u8>,        // interleaved PCM, s16le
    pub frames: usize,
    pub pts: Duration,        // QPC-based, same clock as Frame::pts
    pub format: AudioFormat,  // 48 kHz stereo s16le by default
}
```

Implementations: `WasapiLoopback` (captures the default render endpoint),
`StubAudio` (non-Windows; emits silence so downstream code is testable).

**Why loopback capture matters here.** There is no way to hand ffmpeg a "game audio"
device on Windows: ffmpeg's `dshow` input needs a DirectShow device, and no such
device exists for the default output unless the user installs a virtual audio cable.
Capturing the render endpoint in loopback from Rust and piping raw PCM avoids
requiring the user to install anything.

**Clock.** `Frame::pts` and `AudioBuffer::pts` are both derived from the same QPC
clock, which is what makes A/V alignment tractable at all. Both are real-time
sources consuming at 1×, so drift over a 30 s clip is bounded rather than
accumulating. Drift is logged per clip (§13) but is not a gating criterion for
Phase 1.

### 5.2 `encoder`

Consumes frames, emits an already-compressed stream.

```rust
pub trait Encoder: Send {
    fn configure(&mut self, cfg: &EncodeConfig) -> Result<()>;
    fn submit(&mut self, frame: &Frame) -> Result<()>;
    fn poll_packet(&mut self) -> Result<Option<Packet>>;
    fn flush(&mut self) -> Result<()>;
    fn active_codec(&self) -> VideoCodec;   // for display + ffprobe assertions
}
```

`FfmpegEncoder` spawns the `ffmpeg` sidecar once and streams raw frames to its stdin.
Vendor selection happens at startup via `probe::available_encoders()`, which asks
`ffmpeg -encoders` and confirms the chosen hardware encoder with a 1-frame smoke test.
If no hardware encoder is available the process exits with an actionable error naming
the missing vendor runtime. **There is no silent CPU fallback.**

### 5.3 `replay` — the core

Owns the ring buffer and clip extraction. This is the highest-risk crate and the
focus of the Phase 1 PoC.

```rust
pub struct RingBuffer { /* scratch dir, cap, ledger, ffmpeg child */ }

impl RingBuffer {
    pub fn start(cfg: BufferConfig) -> Result<Self>;
    pub fn trigger(&mut self, at: Instant, trig: Trigger) -> Result<PendingClip>;
    pub fn evict_oldest(&mut self) -> Result<EvictionOutcome>;
    pub fn stats(&self) -> BufferStats;   // bytes_on_disk, segments, span
}

pub struct ClipSplicer { /* concat + lossless remux */ }

impl ClipSplicer {
    pub fn splice(&self, window: SegmentWindow, out: &Path) -> Result<ClipMetadata>;
}
```

`Trigger` is the single entry point for both manual and automatic clipping:

```rust
pub enum Trigger {
    Hotkey,
    GameEvent { kind: EventKind, at: Instant },
}
```

### 5.4 `media`

Thin, explicit driver over the sidecar binaries. No long-lived process; each call is
a short-lived `ffmpeg`/`ffprobe` invocation with a timeout and captured stderr.

- `probe(path) -> MediaInfo` — duration, codec, profile, resolution, bitrate.
- `remux_lossless(in, out)` — `-c copy` + `+faststart`.
- `trim_lossless(in, out, range)` — `-ss`/`-to` + `-c copy`.
- `thumbnail(in, at, out)` — single-frame JPEG at a timestamp.

`trim_lossless` is used for arbitrary user trims in Phase 2; `ClipSplicer` composes
the same primitives for buffer extraction.

### 5.5 `store`

Owns the SQLite schema and migrations. The database file lives in the app data
directory, never in the repo.

```sql
CREATE TABLE sessions (
    id           INTEGER PRIMARY KEY,
    game         TEXT,
    started_at   INTEGER NOT NULL,
    ended_at     INTEGER,
    scratch_dir  TEXT NOT NULL
);

CREATE TABLE clips (
    id           INTEGER PRIMARY KEY,
    session_id   INTEGER REFERENCES sessions(id),
    path         TEXT NOT NULL UNIQUE,
    started_at   INTEGER NOT NULL,
    duration_ms  INTEGER NOT NULL,
    size_bytes   INTEGER NOT NULL,
    codec        TEXT NOT NULL,
    favourite    INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL
);

CREATE TABLE events (
    id           INTEGER PRIMARY KEY,
    session_id   INTEGER REFERENCES sessions(id),
    kind         TEXT NOT NULL,
    at           INTEGER NOT NULL,
    payload      TEXT,
    clip_id      INTEGER REFERENCES clips(id)
);

CREATE INDEX idx_clips_started_at ON clips(started_at);
CREATE INDEX idx_events_session   ON events(session_id, at);
```

`payload` holds integration-specific detail (champion, victim, objective type) as
JSON. Migrations are versioned and forward-only; a downgrade attempt is refused rather
than run.

### 5.6 `events`

See [§7](#7-game-event-integrations).

---

## 6. Data flow

### 6.1 Steady state (buffer running)

```
 [ WGC ]──BGRA frame (QPC pts)──┐
                                ├──▶[ ffmpeg sidecar ]──muxed A/V──▶ scratch/NNNNN.mp4
 [ WASAPI loopback ]──PCM s16───┘     (2 pipe inputs)                      │
                                            │                              │
                                            ├ vendor HW video encoder      │
                                            └ aac audio encoder            ▼
                                                              SegmentLedger (atomic write)
```

The `encoder` crate owns **one** ffmpeg child process with two pipe inputs
(`pipe:0` = raw video, `pipe:3` = raw audio) rather than two processes, so that the
container is muxed once and A/V timestamps stay coherent inside each segment. The
segment muxer therefore cuts video and audio together, and a clip is a set of
already-interleaved segments.

Memory stays flat: the ledger is small and bounded by the cap, and no media is held
in RAM beyond the in-flight frame and audio buffers.

### 6.2 Trigger → clip

```
 hotkey / game event        T = trigger instant
        │
        ▼
 RingBuffer::trigger(at: T, pre: 30s, post: 5s)
        │
        ├─ 1. Record the pending window [T-pre, T+post]
        ├─ 2. Wait until T+post has been written to scratch
        ├─ 3. Resolve the covering segment set from the ledger
        ├─ 4. Trim the leading segment to the exact pre-roll offset (-ss, -c copy)
        ├─ 5. Concat the segments and remux to clip-<ts>.mp4 (+faststart)
        └─ 6. Insert the clips row; commit BEFORE any scratch eviction
```

### 6.3 Keyframe snapping (the central trade-off)

Segments force a keyframe every `segment_time` seconds via
`-force_key_frames expr:gte(t,n_forced*1)`. This is what makes instant, lossless
extraction possible: any segment boundary is a decodable cut point.

The cost, stated plainly:

- Trim accuracy is **≤1 second**, not frame-exact. Frame-exact cuts require
  re-encoding, which principle 5 forbids.
- Forced keyframes cost roughly **5–10% bitrate** at constant quality.

Both are accepted. Phase 2 may reduce snapping by shortening `segment_time` at a
higher keyframe cost; the knob is `BufferConfig::segment_time`.

### 6.4 Why the media is not memory-mapped

The scratch media is written by the ffmpeg child process as plain appended segments.
It is deliberately **not** memory-mapped: mapping a file another process is actively
appending to invites torn reads and stale page-cache views. The properties that
matter — a hard disk ceiling, flat RAM, and survival across a crash — are delivered
by bounded eviction plus the atomically-written `SegmentLedger`.

The ledger is the only memory-mapped artifact, and only because it is small and
written by a single writer using temp-file + rename.

---

## 7. Game event integrations

All integrations are loopback-only. The application performs no outbound network
requests, and this is enforced by construction (see §7.3).

### 7.1 League of Legends — Live Client Data API

Poll `https://127.0.0.1:2999/liveclientdata/allgamedata` once per second while a
game is detected. Riot serves this endpoint with a self-signed certificate, so
certificate verification must be disabled for it.

**Security constraint:** the relaxed TLS configuration is scoped to a *dedicated*
HTTP client that can only address `127.0.0.1:2999`. The global/shared client retains
full verification. A helper that returns the loopback-only client is the sole way to
obtain one; no code path may disable verification on a general-purpose client.

Events derived: kill, death, assist, objective (baron/dragon/herald), game start/end.

### 7.2 CS2 / Dota 2 — Game State Integration

A `127.0.0.1`-bound HTTP listener consumes Valve GSI POSTs. The listener:

- Binds loopback only; binding a non-loopback address is refused.
- Requires a generated shared token, written alongside the GSI config the user
  installs. Requests without a valid token are rejected and dropped.
- Rejects malformed bodies rather than logging them wholesale.

The `gamestate_integration_localplay.cfg` file is generated by the app and installed
by the user; the app does not write into game installation directories on its own.

### 7.3 Enforcement of "zero egress"

An integration test asserts that the workspace contains no outbound-HTTP client
construction outside the loopback-scoped LoL module, and that the GSI listener binds
`127.0.0.1`. This is a static check over the source tree, not a runtime hope.

### 7.4 Generic games

`RegisterHotKey` on `Ctrl+F8` (configurable). Produces `Trigger::Hotkey`. This path
requires no game integration and is the fallback that makes localplay universally
applicable.

---

## 8. Storage manager

### 8.1 Policy

Two independent rules, evaluated at startup and on a timer:

1. **Total size cap.** Evict non-favourited clips least-recently-used first until
   total bytes are under `storage.max_total_bytes`.
2. **Age limit.** Delete non-favourited clips older than `storage.max_age_days`.

Favourited clips are exempt from both rules. If applying the rules cannot bring
storage under the cap because favourites alone exceed it, the manager logs a warning
and stops — it never deletes a favourite, and never deletes a clip to satisfy a cap
it cannot meet.

### 8.2 Deletion ordering

The database row is deleted and committed **before** the file is unlinked.

- If the unlink fails, the file is orphaned on disk and swept by a later pass.
- The reverse order would leave rows pointing at deleted files, which is unrecoverable
  from the user's perspective.

**No file is deleted unless a committed database row names it.** A sweep only
considers paths present in `clips`.

---

## 9. Session review UI (Phase 2)

Out of scope for Phase 1, specified here so the crates are shaped to support it.

- Dark-mode-only UI: session list, clip list, timeline scrubber with event markers.
- Playback uses a `<video>` element fed by Tauri's asset protocol, scoped strictly to
  the clips directory. Range requests provide scrubbing without custom streaming code.
- The trim control maps to `media::trim_lossless`; the UI never re-encodes.
- Event markers are rendered from the `events` table, joined to clips by timestamp.

---

## 10. Configuration

A single TOML file in the application data directory, with `config.example.toml`
committed to the repo. Environment variables are deliberately not used for
configuration (the `.gitignore` ignores `.env*` so secrets never land in the tree).

```toml
[buffer]
pre_seconds   = 30      # replay window before the trigger
post_seconds  = 5       # continue recording after the trigger
segment_time  = 1       # seconds per scratch segment; sets keyframe granularity
scratch_cap_bytes = 2147483648   # 2 GiB hard ceiling on the scratch directory
scratch_dir   = ""      # empty = %LOCALAPPDATA%\localplay\scratch

[encode]
vendor        = "auto"  # auto | nvenc | qsv | amf
codec         = "h264"  # h264 | hevc
bitrate_kbps  = 20000
fps           = 60
output_size   = ""      # empty = native capture resolution

[audio]
enabled       = true
source        = "loopback"   # default render endpoint, captured in loopback
codec         = "aac"
bitrate_kbps  = 192

[storage]
clips_dir         = ""  # empty = %LOCALAPPDATA%\localplay\clips
max_total_bytes   = 53687091200   # 50 GiB
max_age_days      = 7

[hotkeys]
clip = "Ctrl+F8"

[events]
lol_poll_enabled = true
gsi_port         = 45671
```

---

## 11. PoC scope & success criteria

**Deliverable:** `localplay-cli buffer` — a headless Windows binary that runs the
full pipeline.

**In scope:** WGC capture + WASAPI loopback audio → hardware encode → bounded segment
ring → hotkey trigger → lossless clip extraction → SQLite row.

**Out of scope:** GUI, session review, storage cleanup policies, game integrations.
The `Trigger` enum is present but only `Trigger::Hotkey` is wired.

| # | Criterion | Verification method |
|---|---|---|
| 1 | Captures the primary monitor via WGC at the configured fps | Startup log + frame counter over the run |
| 2 | Scratch directory stays at or under `scratch_cap_bytes` | Asserted on `BufferStats` after a 5-minute soak |
| 3 | `Ctrl+F8` writes `clip-*.mp4` within 2 s of post-roll completion | Timestamp delta, printed by the run |
| 4 | Clip duration matches `pre_seconds + post_seconds` ± 0.5 s | `ffprobe -show_format` |
| 5 | Clip is a stream copy, not a re-encode | `ffprobe` codec/profile matches the live encoder; export completes near-instantly |
| 6 | Steady-state CPU under 5% of one core and RSS under 400 MB | Process counters sampled during the soak |
| 7 | The process exits non-zero with an actionable message when no hardware encoder exists | Run on a machine/GPU without one, or force `vendor` to an absent encoder |
| 8 | The clip contains a synchronised audio stream | `ffprobe -show_streams` reports one video and one audio stream; the audio is audible and lip-sync is correct on playback; A/V drift is logged |

Criteria 3–6 are the ones that prove the design. Criteria 1–2 prove the plumbing.
Criterion 7 exists because a silent CPU fallback would violate principle 3. Criterion 8
exists because a clip with no audio is not shippable, and A/V sync is the failure mode
that a video-only PoC would have hidden.

### 11.1 Test strategy

| Level | Scope | Runs where |
|---|---|---|
| Unit | Ledger eviction math, window resolution, config parsing, cleanup policy rules | Any platform |
| Integration | `CaptureBackend` via `StubCapture` → encode → buffer → splice, using a short synthetic clip | Any platform (stub + ffmpeg) |
| Integration | Lossless trim determinism: assert output codec/bitrate profile equals input | Any platform with ffmpeg |
| End-to-end | Criteria 1–7 above | Windows only |

Non-Windows CI runs the unit and stub-integration tiers only. This is a real gap and
is recorded in §12.

---

## 12. Verification constraints

The development host is **macOS arm64**. Consequences, stated so they are not
mistaken for completeness:

- The WGC and DXGI backends **cannot be compiled or executed** on the development
  host. They are gated behind `#[cfg(windows)]`, and `StubCapture` provides a
  non-Windows implementation so the workspace type-checks and the unit/stub tiers run.
- Success criteria 1–4, 6, and 7 **can only be executed on Windows hardware.** No
  amount of local work substitutes for this.
- Phase 1 is not "done" on the strength of a green local test run. It is done when the
  criteria are executed on Windows and the measured numbers are recorded.

Hardware assumed for the criteria: a GPU with NVENC, QuickSync, or AMF, and an NVMe
scratch volume.

---

## 13. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| Windows path unverifiable on this host | PoC could be broken on first run | Keep Windows-specific surface small; `StubCapture` keeps the rest tested; verify explicitly on Windows before declaring done |
| Vendor encoder detection is fiddly (QSV/AMF especially) | Startup failure or wrong encoder | Probe with `ffmpeg -encoders` plus a 1-frame smoke test; fail with a message naming the missing runtime |
| Forced 1 s keyframes raise bitrate 5–10% | Larger clips at equal quality | Accepted; `segment_time` is configurable |
| ffmpeg sidecar supply chain | A tampered binary would run with user privileges | Pin a version and verify a checksum in `xtask`; record the source URL in the manifest |
| A/V drift between the QPC video clock and the WASAPI audio clock | Desynced clip, the classic capture bug | Both sources share the QPC clock and run at 1×, so drift is bounded not cumulative; drift is logged per clip. WasapiLoopback resamples to a fixed 48 kHz so the audio timeline is exactly derivable |
| Loopback captures *all* system audio, not just the game | Discord/music bleed into clips | Accepted for Phase 1 — it is the same behavior OBS defaults to. Per-application isolation requires WASAPI process loopback (`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, Win10 2004+) and is deferred to Phase 2 |
| WGC unavailable on some configurations | No capture | DXGI fallback; typed error if neither works |
| Sidecar + child-process management | Zombie ffmpeg processes on crash | Explicit `Child` ownership, kill-on-drop, and a startup sweep for orphaned processes from a previous run |

---

## 14. Roadmap

- [ ] **Phase 0** — repo, tooling, this spec
- [ ] **Phase 1** — WGC capture + hardware encode + bounded ring + hotkey trigger (this spec)
- [ ] **Phase 2** — Tauri shell, timeline scrubber, lossless trim/export UI
- [ ] **Phase 3** — storage manager, auto-cleanup policies, favourites
- [ ] **Phase 4** — LoL Live Client + CS2/Dota 2 GSI integrations
- [ ] **Phase 5** — full-session recording, chapter marks, installer/packaging

---

## 15. Out of scope (explicitly)

- Cloud upload, sharing, or accounts of any kind.
- macOS or Linux capture backends.
- Audio *mixing*: per-application isolation, separate microphone and game tracks, and
  any mixing or ducking. Phase 1 captures the default render endpoint in loopback as a
  single mixed track, which is deliberate and matches what OBS does by default.
- Overlay rendering or in-game HUD.
- Auto-update mechanism.
