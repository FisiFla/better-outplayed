# Verification status

**This document is a ledger of what has actually been proven about localplay, and how.**
It exists because a repository that only says what it *intends* to do will be believed,
and this project has already shipped a feature that passed every macOS test and produced
solid black video the first time it ran on real Windows hardware. That memory is the
reason this file is blunt.

Read it before believing any other document in this tree. Where a source is quoted it is
named, so every line below can be re-checked.

---

## The headline, stated plainly

> **Almost none of the Windows-only surface has ever executed on Windows.**
> Exactly one capture session has ever run on a real Windows box (2026-09-23, RTX 3090,
> 3840x2160 at 150% scaling). It proved the capture→encode→ring-buffer→splice path can
> run, and it found that **the pipeline cannot sustain the configured frame rate at 4K
> and burns ~50% of a CPU core doing it** ([issue #1]). Everything written after that
> session — the readback skip, the WASAPI engine-side sample-rate conversion, the whole
> recording engine, the desktop GUI's recording wiring, the storage policy, both Phase 4
> game integrations and the sidecar pipeline — is **type-checked for
> `x86_64-pc-windows-msvc` or tested on macOS, and has never run on Windows.** The two
> game integrations have never been run against a game at all.

If that paragraph surprises you, it is doing its job. The rest of this file is the detail.

[issue #1]: https://github.com/FisiFla/localplay/issues/1
[issue #2]: https://github.com/FisiFla/localplay/issues/2

---

## How to read the evidence levels

Every claim below carries one of these four labels. They are not a scale of confidence;
they are a statement about *what was done*.

| Level | Meaning |
|---|---|
| **Verified on Windows hardware** | It ran on a real Windows machine and produced the stated result, at the stated number. |
| **Verified on the dev host** | Covered by an automated test or CI job on macOS (the development host). Says nothing about Windows behaviour. |
| **Type-checked only** | Compiled for `x86_64-pc-windows-msvc` with `cargo check` (no link, no run). Proves the API shape, ownership and types; proves nothing about runtime behaviour. |
| **Unverified** | Written, but there is no test, no CI job and no observed run of any kind. |

A test existing for *adjacent* code does **not** raise an item's level. Where the
repository contradicts itself, that is called out rather than resolved by preference.

---

## Ledger tallies

Counting the distinct significant claims about the system made in §1–§4, each once at its
**strongest** level (an item named in more than one table is counted once):

| Level | Count |
|---|---|
| Verified on Windows hardware | 16 |
| Verified on the dev host | 18 |
| Type-checked only | 5 |
| Unverified | 3 |

The per-item tables below are the source of truth; the tally is a summary. The three
**Unverified** items are: the scratch-cap value used in the session (no cap number is
recorded), whether the session's audio was non-silent (nobody recorded listening), and
whether the WGC `copy_out` fix produces real pixels (the repository contradicts itself —
see §2).

---

## 1. The one Windows capture session (2026-09-23)

Environment: **RTX 3090, Intel i7-13700K, 3840x2160 primary display at 150% DPI scaling**,
configured `encode.fps = 30`, `Ctrl+F8` hotkey, H.264. The measurements below are recorded
in [issue #1] and [issue #2]; the harness's own argument list is quoted in issue #2.

Command used to check the source of every row: `gh issue view 1` / `gh issue view 2`.

| Claim | Level | Evidence / number |
|---|---|---|
| WGC capture starts and delivers frames at the monitor's native **3840x2160** | Verified on Windows hardware | issue #1 measurement table, "Capture resolution — 3840x2160, real content — PASS" |
| The encoder child runs with **`h264_nvenc`** | Verified on Windows hardware | issue #2, the encoder's actual argument list: `-c:v h264_nvenc -b:v 20000k -g 30 …`; also issue #1 (RTX 3090 → NVENC) |
| Audio is transported to the encoder over **loopback TCP** and muxed as **48 kHz stereo AAC** | Verified on Windows hardware | issue #2 arg list: `-f s16le -ar 48000 -ac 2 -i tcp://127.0.0.1:51234 … -c:a aac -b:a 192k` |
| The ring buffer writes segments | Verified on Windows hardware | issue #1: `frames=1814 segments=57 bytes=37795772 span=57000ms dropped=1183 …`; issue #2: segments every ~1.25 s |
| A `Ctrl+F8` clip is produced and its **duration matches the target** | Verified on Windows hardware | issue #2: "a clip of **13,091 ms** was produced against a **13,000 ms** target, written **137 ms** after the post-roll elapsed" |
| Acceptance criteria **2, 3, 4, 5, 7 and 8 pass** on this hardware | Verified on Windows hardware | issue #1: "clips are produced correctly, and criteria 2, 3, 4, 5, 7 and 8 all pass" |
| Encoder probing reports an **`advertised, FAILS`** vendor (an encoder ffmpeg lists but the machine cannot open) | Verified on Windows hardware | `xtask/src/main.rs` comment: "measured on a box with no AMD hardware, ffmpeg listed `h264_amf` and died with `DLL amfrt64.dll failed to open`" |
| The clip's **video codec is H.264** and **audio is AAC 48 kHz stereo** | Verified on Windows hardware (see caveat) | follows from criteria 4/5/8 passing (issue #1) + the fixed pipeline format; **but** the specific claim "the audio was non-silent" is **not recorded anywhere** — neither the repo nor the issues state that anyone listened. Treat non-silence as **Unverified**. |
| The scratch-**cap** eviction ran under a **15 MB cap** and held | **Unverified** | issue #1 records criterion 2 as PASS, but **no cap value is recorded anywhere** (not a 15 MB figure, not any other). "15 MB" does not appear in the repository, its git history, or either issue. |

### What **failed** on that hardware — do not soften these

| Failure | Level | Number |
|---|---|---|
| Cannot sustain the configured frame rate at 4K | Verified on Windows hardware | **~24 fps** sustained against **30 fps** configured — issue #1 |
| Frames dropped by the encoder | Verified on Windows hardware | **~45% of delivered frames**; the debug line reads `dropped=1183` of ~3000 delivered — issue #1 |
| CPU cost | Verified on Windows hardware | **50.8% of one core** against a **< 5%** target — issue #1 (criterion 6 **FAIL**) |
| The media timeline runs slower than real time | Verified on Windows hardware | **0.81 s of media per 1 s of wall clock** (segments written at 0.81/s, each carrying exactly 1.000000 s) — issues #1 and #2 |
| RSS (the one criterion-6 half that passed) | Verified on Windows hardware | 235 MB, peak 267 MB against a < 400 MB target — issue #1 |

Accepted consequence: because media time runs at 0.81x, **a `pre_seconds` clip covers more
real seconds than configured** — `pre_seconds = 10` yields about **12.3 real seconds**
(issue #2). This is a fidelity gap, not currently a bug: the trigger works in media time,
so clips are correct, they are simply longer in wall-clock terms than the setting implies.

`gh issue list --state all` shows both issues still **OPEN** — neither the 4K throughput
shortfall nor the timeline divergence has been fixed and re-measured on hardware. The
readback-skip commit ([`dd921b3`](#4-added-since-the-windows-session)) was the first
attempt at issue #1 and says in its own message that it was not measured on Windows.

---

## 2. The capture path, item by item

Source checked with `git log --oneline` and by reading each module's own doc comment.

| Path element | Level | Evidence |
|---|---|---|
| **WGC video capture** (`crates/capture/src/wgc.rs`) — starts, delivers frames, pulls via `TryGetNextFrame` | Verified on Windows hardware | ran in the session (issue #1); the module doc records "It was run on Windows 11 for the first time on a 4K/150%-scaled desktop" |
| **WGC `copy_out` correctness** (copy the captured texture to staging *before* `Map`) | **Disputed in-tree** | `wgc.rs` module doc says the first run produced **pure black frames** because `copy_out` mapped a staging texture without `CopyResource`, and that "**neither fix has been re-run on Windows** … whether frames now carry real pixels is unverified at runtime". issue #1's table says "real content — PASS". **These two statements contradict each other and the repository does not resolve which is current.** Do not assume real pixels without re-running. |
| **WGC `discard_pending`** — close surplus frames without the GPU readback (`dd921b3`) | Type-checked only | `dd921b3` message: "NOT measured here: there is no Windows host" |
| **WASAPI loopback audio** (`crates/capture/src/wasapi.rs`) — endpoint-native format path | Verified on Windows hardware | the session produced clips with audio and criterion 8 passed (issue #1) |
| **WASAPI engine-side sample-rate conversion** (`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`, `0848fc7`) | Type-checked only | `0848fc7` message: "UNVERIFIED at runtime … nothing here has ever executed on Windows" |
| **The silence detector** for a converted-but-silent endpoint (`2a63949`) | Type-checked only | it only fires on the path above, which has never run; `crates/capture/src/lib.rs` says the failure mode is "external and **unverified at runtime**" |
| **Encoder child** (`crates/encoder/src/ffmpeg.rs`, ffmpeg sidecar) — H.264 via hardware encoder | Verified on Windows hardware | `h264_nvenc` in issue #2's arg list; criteria 4/5 passed |
| **Audio over loopback TCP** (`47cd974`) | Verified on Windows hardware | `tcp://127.0.0.1:51234` in issue #2's arg list |
| **Hardware-encoder selection + smoke test** (`6e63977`, `xtask probe`) | Verified on Windows hardware | the `advertised, FAILS` case was observed on the box (see §1) |
| **`Frame` move instead of clone on submit** (`f6a5a0e`) | Type-checked only | `f6a5a0e` message: "not measured here — there is no Windows host to measure it on" |
| **Ring buffer, segment ledger and cap eviction** (`crates/replay`) | Verified on Windows hardware | segments written; criterion 2 passed (issue #1) — though **not** at any recorded cap value (§1) |
| **Clip splicing** (`crates/replay`, `crates/media`) — lossless `-c copy` remux | Verified on Windows hardware | the 13,091 ms clip; criteria 4 and 5 passed (issues #1, #2) |
| **The media-time trigger** (`55f1541`) | Verified on Windows hardware | fixed and confirmed on the box (issue #2: "Verified on hardware") |

### Tests that cover the non-Windows half of this path

These run on macOS and are the only automated evidence for the logic above. None of them
executes a Windows API.

| Claim | Level | Test |
|---|---|---|
| The pacer discards surplus frames **without** reading them back | Verified on the dev host | `apps/localplay-cli/tests/discard_readback.rs::a_source_faster_than_the_pacer_is_discarded_without_being_read_back` |
| A frame the source offers while the pacer is not due is discarded, not materialised | Verified on the dev host | `discard_readback.rs` (first test) |
| The stub backend's discard path counts separately from its readbacks | Verified on the dev host | `crates/capture/src/stub.rs` unit tests |
| Pipe size is reconciled with the backend's native size | Verified on the dev host | `b94be7b` message: "a macOS run logs `-s=1280x720 (capture native)`"; cross-checked for `x86_64-pc-windows-msvc` |

---

## 3. Everything else in the tree

| Subsystem | Level | Evidence |
|---|---|---|
| `localplay-media` (`ffprobe` → `MediaInfo`, lossless remux/trim) | Verified on the dev host | `crates/media/tests/lossless.rs`, `crates/media/src/probe.rs` (real ffmpeg) |
| `localplay-store` (SQLite schema, clip/session/event index) | Verified on the dev host | `crates/store/src/lib.rs` tests, real SQLite |
| **Storage cleanup policy + executor** delete-before-unlink (`48b5e5a`) | Verified on the dev host | `crates/store/src/cleanup.rs::the_size_rule_evicts_oldest_first_until_under_the_cap`, `crates/store/tests/cleanup_ordering.rs` |
| Evicting a clip detaches its events instead of failing the foreign key | Verified on the dev host | `crates/store/src/lib.rs::deleting_a_clip_releases_its_events_instead_of_failing` |
| `localplay-recorder` — the recording engine shared by CLI and GUI (`38ed353`) | Verified on the dev host | `crates/recorder/src/tests.rs::a_clip_records_why_it_was_taken` (real recording, real ffmpeg, real SQLite) |
| `localplay-cli` drives the engine (`667a2e8`) | Verified on the dev host | CLI integration tests in `apps/localplay-cli/tests/` |
| Desktop IPC command layer + session-review window (`e77c061`, `2528285`) | Verified on the dev host | desktop-shell Rust tests (run in CI, `apps/desktop/src-tauri`) |
| Desktop **recording wiring** — Start/Stop/Save clip over the engine (`ce81aef`, `bf87be0`) | Verified on the dev host (headless) | frontend `vitest` (`apps/desktop/src/lib/recording.test.ts` etc.), shell Rust tests, and headless render screenshots (`2f0be65`). **No GUI window is opened.** |
| **Sidecar pipeline** (`07b9d83`) — fetch + SHA-256 verify + allowlisted extract | Verified on the dev host | `xtask/src/sidecars.rs` unit tests (zip-slip rejected, symlink rejected, mismatch refused, allowlist honoured) |
| **Phase 4 — LoL Live Client poller** (`3ae0a01`) | Verified on the dev host (mocks only) | `crates/events/tests/lol_mock.rs` drives a real TLS server on loopback; see §5 |
| **Phase 4 — CS2/Dota 2 GSI listener** (`32774ff`) | Verified on the dev host (mocks only) | `crates/events/tests/gsi_listener.rs` over real loopback sockets |
| **Phase 4 — event-triggered clip + reason** (`71922a4`) | Verified on the dev host | `crates/recorder/src/tests.rs::a_clip_records_why_it_was_taken` |
| **GSI accept-loop blocking-mode fix** (`c5abc58`) | Verified on the dev host (caught by CI) | see §6 |

---

## 4. Added since the Windows session

These are the changes committed **after** the capture session's measurements. None of them
has run on Windows; the ones marked "type-checked only" have not run *anywhere* off the
compiler. Establish the boundary yourself with:

```console
$ git log --oneline --format='%h %ci %s'
```

The first commit after the session is `dd921b3` (2026-09-23 18:00).

| Change | Commit | Level |
|---|---|---|
| Skip the GPU readback for frames the pacer discards (`discard_pending` + pacer-before-materialise) | `dd921b3` | Type-checked only |
| Store cleanup policy + executor | `48b5e5a` | Verified on the dev host |
| CLI indexes clips and enforces the storage policy | `d49e5ee` | Verified on the dev host |
| Tauri v2 shell scaffold | `686081f` | Verified on the dev host |
| Desktop IPC command layer | `e77c061` | Verified on the dev host |
| Desktop session-review window (list, player, scrubber, trim) | `2528285` | Verified on the dev host |
| `xtask` sidecar fetch/verify | `07b9d83` | Verified on the dev host |
| Headless window render + screenshots | `2f0be65` | Verified on the dev host |
| Recording engine lifted out of the CLI | `38ed353` | Verified on the dev host |
| CLI drives the engine | `667a2e8` | Verified on the dev host |
| Desktop recording wiring (start/stop/clip/status) | `ce81aef`, `bf87be0` | Verified on the dev host (headless) |
| Phase 4 LoL poller | `3ae0a01` | Verified on the dev host (mock only) |
| Phase 4 GSI listener | `32774ff` | Verified on the dev host (mock only) |
| Event-triggered clip + reason recording | `71922a4` | Verified on the dev host |
| Phase 4 documentation | `3a4e75f` | n/a (prose) |
| WASAPI autoconversion | `0848fc7` | Type-checked only |
| Silence detector | `2a63949` | Type-checked only |
| CI workflow (tests + Windows cross-check + frontend) | `42b68a9` | Verified on the dev host |
| GSI accept-loop blocking-mode fix | `c5abc58` | Verified on the dev host (CI) |

---

## 5. What CI covers, and what it does **not**

CI (`.github/workflows/ci.yml`) runs on **`macos-latest` only** — the development host.
Check it yourself:

```console
$ gh run list --limit 10
```

What a green run proves:

- the whole Rust **workspace test suite** (with `localplay-encoder/test-encoders`) — 319
  tests, `cargo test --workspace --features localplay-encoder/test-encoders`;
- **`cargo check` for `x86_64-pc-windows-msvc`** of the cross-checkable crates
  (`capture`, `encoder`, `events`, `replay`, `media`, `xtask`) — *type-checking*, not
  running. No MSVC linker is needed because `check` stops before linking;
- the desktop frontend: `npm ci`, `svelte-check`, `vitest`, production build; and then the
  desktop shell's own Rust tests;
- `cargo clippy` over the workspace (warnings surfaced, not fatal).

What a green run does **not** prove — quoted from the workflow's own header:

- **No Windows runtime verification.** "Not one line of the WASAPI or WGC backends is
  *executed* here; they are only compiled. A green run says nothing about how the app
  behaves on a real machine."
- **No GUI window.** "No window is opened, the screen is never captured and no input is
  synthesised."
- **`localplay-store`, `localplay-recorder`, `localplay-cli` and the desktop shell are
  never cross-checked for Windows at all**, because rusqlite's bundled SQLite C source
  cannot be built for MSVC from macOS. They are host-verified only, and they "cannot be
  cross-checked anywhere here."

---

## 6. Platform traps

A recurring class of bug that Linux and Windows do **not** have, and which has already
bitten this codebase twice. The full note is in
[`docs/platform-traps.md`](platform-traps.md); the short version:

> **`accept()` makes the accepted socket inherit the listener's `O_NONBLOCK` flag on
> macOS/BSD.** A listener is put into non-blocking mode only so an accept loop can enforce
> a deadline — but on macOS the *accepted* socket comes back non-blocking too, so reads and
> writes fail with `WouldBlock` at the worst moment. **After `accept()`, always set the
> socket's blocking mode explicitly; never assume it.**

Two sites, both fixed, both with the reasoning at the call site:

- `crates/encoder/src/ffmpeg.rs::accept_within` — the loopback-TCP audio transport
  (`stream.set_nonblocking(false)` after accept; fix `82578bd`).
- `crates/events/src/gsi.rs::handle` — the GSI listener (`stream.set_nonblocking(false)`;
  fix `c5abc58`). This second instance was caught **only by CI on a fresh runner** — it
  does not reproduce reliably on a warm local run:

```console
$ gh run view 35904179918 --log-failed
… test a_post_with_no_auth_block_at_all_is_rejected … FAILED
…   left: 400
…  right: 403
```

`crates/events/tests/lol_mock.rs` carries the same guard for the same reason.

---

## 7. Known limitations — do not mistake these for bugs, or for working features

- **The media timeline runs below real time on 4K hardware** (0.81x measured; see §1). The
  consequence: **a `pre_seconds`-long clip corresponds to more real seconds than
  configured** (`pre_seconds = 10` → ~12.3 real seconds). Documented in issue #2; not fixed.
- **Criteria 1 and 6 fail on 4K hardware** — the pipeline cannot sustain 30 fps and costs
  ~50.8% of a core. Open in issue #1.
- **Single-monitor capture only.** `crates/capture/src/wgc.rs` captures the primary monitor
  (index 0) and rejects any other index; monitor selection is not wired to config.
- **No macOS/Linux capture backend.** `crates/capture/src/platform.rs` selects WGC/WASAPI
  on Windows and the synthetic `StubCapture` everywhere else. Off Windows the pipeline
  captures a generated image and *proves nothing* about real capture.
- **The capture frame pool is never recreated on a display-mode change**
  (`Direct3D11CaptureFramePool::Recreate` is not called); a mid-capture resolution or
  refresh-rate change is unhandled beyond refusing a frame whose size diverged.
- **Encoder probing is a 1-frame smoke test, not a live capture.** `xtask probe` proves an
  encoder *opens* on this machine; it does not prove the live 4K path drives it.
- **Highlights-vs-markers is a product policy, not a guarantee.** Kills/deaths/objectives/
  bomb/game-end take a clip; game/round start/end are recorded as markers with no footage.
  The split is one `match` arm (`EventKind::is_highlight`) and is the first thing to revisit
  if the output is not what you want.
- **No coalescing of event bursts.** A triple kill or a bomb plus a round end inside one
  post-roll is clipped **sequentially** — one clip per highlight, each with its own
  pre-roll. Events wait in the channel; they are not de-bounced (see the Phase 4 note §3).
- **The event integrations speak hand-rolled HTTP/1.1** (`crates/events/src/wire.rs`,
  ~250 lines with its own tests) rather than a general-purpose stack. The bounds that
  matter are tested, but it is not a battle-tested client.
- **Live-source A/V clock divergence is unmeasured.** Only within-clip drift is logged; the
  divergence between the WGC video clock and the WASAPI audio clock is not instrumented
  (runbook "Known gaps").
- **The Phase 4 integrations have never been run against a game.** No League client and no
  CS2/Dota 2 client was ever contacted; the payloads are canned fixtures and the servers are
  mocks in the test process (see `docs/plans/2026-09-23-localplay-phase-4-integrations.md`
  §5 for the full, itemised list of what that leaves unproven).

---

## 8. What to run on Windows next

The smallest ordered set of runs that moves the most items from *type-checked* to
*verified*, from cheapest to most informative. Each is a single command plus an
observation; the Phase 1 runbook
(`docs/runbooks/phase-1-verification.md`) already contains the PASS/FAIL detail.

1. **`cargo xtask probe`** — confirms which hardware encoders this box can actually open.
   Cheap, no capture. Confirms the encoder half of the path.
2. **`cargo run --release -p localplay-cli -- buffer`** on the 4K display for ~40 s, then
   read the debug line. Look for: **`skipped=` non-zero** (the readback skip, `dd921b3`)
   and **`dropped=` falling to zero** with **`fps=` reaching the configured rate** — this is
   the *only* thing that can neither be tested nor type-checked for the readback skip. Also
   re-measures **criteria 1 and 6** (issue #1).
3. **Capture one `Ctrl+F8` clip and `ffprobe` it** — re-confirms criteria 3/4/5/8 on the
   current build, and is the only way to settle the **black-frame dispute** in §2: play the
   clip and confirm real pixels.
4. **Repeat run 2 at 1080p.** issue #1 asks explicitly whether the criteria pass at 1080p;
   that separates "4K is hard" from "the pipeline is slow", and it is the measurement the
   acceptance thresholds were actually written for.
5. **Set a low `scratch_cap_bytes` and soak 5 minutes** — re-confirms criterion 2 eviction
   *and records the cap value that was used* (which §1 could not find).
6. **Force a `vendor` that is not present** — confirms criterion 7's fail-loud path on the
   current build.
7. **Set the Windows default playback device to 44.1 kHz or 96 kHz and record a clip** —
   exercises the WASAPI autoconversion (`0848fc7`) and the silence detector (`2a63949`),
   neither of which has ever run.
8. **Launch the desktop GUI on Windows and press Start / Save clip** — the entire GUI
   recording path (`ce81aef`, `bf87be0`) has only ever run headlessly.
9. **Play one bot game (League) and one CS2 round** — the last step for the Phase 4
   integrations, and the only one that can validate the payload shapes, the TLS handshake
   with Riot's certificate, and the GSI token echo.
