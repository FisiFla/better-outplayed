# Verification status

**This document is a ledger of what has actually been proven about localplay, and how.**
It exists because a repository that only says what it *intends* to do will be believed,
and this project has already shipped a feature that passed every macOS test and produced
solid black video the first time it ran on real Windows hardware. (That defect has since
been fixed and re-confirmed to produce real pixels — §2.) That memory is the reason this
file is blunt.

Read it before believing any other document in this tree. Where a source is quoted it is
named, so every line below can be re-checked.

---

## The headline, stated plainly

> **Almost none of the Windows-only surface has ever executed on Windows.**
> A small number of capture runs have now executed on real Windows boxes. The one whose
> measurements are recorded in-tree is the 2026-09-23 session (RTX 3090, 3840x2160 at
> 150% scaling, [issue #1]): it proved the capture→encode→ring-buffer→splice path can run,
> and found that **the pipeline cannot sustain the configured frame rate at 4K and burns
> ~50% of a CPU core doing it** ([issue #1]). A run with a 15 MB cap left a scratch ring on
> the box that was re-read (read-only) after the fact; that re-read re-confirms the path — real
> pixels, non-silent audio, a cap that held and evicted oldest-first — and resolves the
> three items this ledger previously could not verify (§1). Everything written after that
> first session — the readback skip, the WASAPI engine-side sample-rate conversion, the
> whole recording engine, the desktop GUI's recording wiring, the storage policy, both
> Phase 4 game integrations and the sidecar pipeline — is **type-checked for
> `x86_64-pc-windows-msvc` or tested on macOS, and has never run on Windows.** The two
> game integrations have never been run against a game at all.

If that paragraph surprises you, it is doing its job. The rest of this file is the detail.

**How to move an item out of *type-checked only*:** run `cargo xtask verify`
(`07b90fc`) on the Windows box. It runs the criteria it can automate in one bounded
session and writes `target\verify\report.md` — every check with its threshold, its
measured value and the raw output behind it — marking anything it could not perform as
*not performed*, with the reason. From an SSH session, use
`scripts/verify-in-interactive-session.ps1`, because a capture session needs a desktop.
**The harness itself has never run on Windows** (it was exercised on the development host
against the synthetic backends), so its own report is part of what you are checking: the
first row to look at is `capture_started` — if the WGC line is missing, or the geometry is
`1280x720`, the session has no desktop (or the stub ran) and the rest of that report says
nothing. It cannot check the GUI, a real keypress, anything needing a real game, or the
live clock divergence; §9 lists what stays manual.

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
| Verified on Windows hardware | 19 |
| Verified on the dev host | 23 |
| Type-checked only | 6 |
| Unverified | 0 |

The per-item tables below are the source of truth; the tally is a summary. Three items the
previous pass could not confirm — the scratch-cap value, whether the audio was non-silent,
and whether the WGC `copy_out` fix produces real pixels — have since been **verified** by a
read-only re-read of files retained on the box (see §1). They move from *Unverified* to
*Verified on Windows hardware*: 16 → 19 Verified, 3 → 0 Unverified. The desktop shell's
background half (§3, §8) then added three claims of its own: two at *verified on the dev
host* — the background logic, and the trigger path driven through a real recording — and one
at *type-checked only* — carrying `RegisterHotKey`'s failure back to the caller, which needs
a Windows message queue to run. The measured-rate change (§4) added one at *verified on
the dev host* — the rate decision, its new config key and the probe's own bounds are tested,
and the startup lines were produced by real runs — while the *accuracy* of its measurement at
4K on the target hardware was explicitly not claimed. The timeline fix (§1, §4) adds two more
at the same level — that the media clock is the frames' arrival timestamps (a local A/B, a
regression test that fails without it), and that the VFR output still splices, probes and
plays (a splice test that decodes every frame) — and *demotes* the measured-rate claim to a
pacing aid rather than a timeline guarantee: 20 → 23 Verified on the dev host.

---

## 1. The Windows capture runs and the retained ring

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
| The clip's **video codec is H.264** and **audio is AAC 48 kHz stereo** | Verified on Windows hardware | follows from criteria 4/5/8 passing (issue #1) + the fixed pipeline format; and the audio is confirmed **non-silent** by measurement — the retained clip probed `mean_volume = -31.7 dB, max_volume = -11.3 dB` (a silent track would sit near -91 dB) — see the read-only note below. |
| The scratch-**cap** eviction ran and held | Verified on Windows hardware | a **15,000,000-byte** cap with a ledger `total_bytes = 14880356` (≤ cap) and 24 segments on disk, `seg-000063.mp4` … `seg-000086.mp4` — 63 segments evicted, oldest-first. Re-derived read-only from the retained ring, not from a committed log (see below). |

### Re-derived read-only from files still on the box

When this repository was inspected there was no committed log carrying these numbers; the
fresh values below come from **read-only analysis of the scratch ring and one clip still
present on the Windows box** (`%LOCALAPPDATA%\localplay\scratch` and `clips`) using
`ffprobe`/`ffmpeg` reads only — no capture, no display, no input. The run-level `Ctrl+F8`
clip facts come from the earlier **interactive session's own output**, which was never
committed; that provenance is weaker than a committed log and is labelled as such.

| Claim | Level | Measured |
|---|---|---|
| The cap eviction holds and deletes oldest-first | Verified on Windows hardware (read-only re-read) | cap **15,000,000 B**; ledger `total_bytes = 14880356`; 24 segments, lowest `seg-000063.mp4`, highest `seg-000086.mp4`; **63 segments evicted**, oldest first |
| Segments carry real pixels, not black frames | Verified on Windows hardware (read-only re-read) | `seg-000084.mp4` = 640072 bytes; video h264 **3840x2160**; a decoded frame (downscaled 320x180) has R/G/B means **16.9 / 25.3 / 42.0** with **216 distinct colours** (a uniform frame would be 1); frame exported as an 80798-byte PNG and **visually confirmed to show a real desktop**. The `copy_out` black-frame defect is therefore **fixed and producing real content** (§2) |
| Segments carry non-silent audio | Verified on Windows hardware (read-only re-read) | `seg-000084.mp4` audio: aac 48000 Hz, 2 channels; `mean_volume = -52.5 dB, max_volume = -42.0 dB` (digital silence is ≈ -91 dB) |
| Segments carry a fixed number of frames | Verified on Windows hardware (read-only re-read) | 30 frames per segment |
| A `Ctrl+F8` clip is produced, correct duration, non-silent | Verified on Windows hardware (interactive session) | **13,091 ms** against a **13,000 ms** target (10 s pre + 3 s post), written **137 ms** after the post-roll elapsed; probed **h264 High 3840x2160 + aac LC 48 kHz stereo, 8,529,609 bytes**; audio `mean_volume = -31.7 dB, max_volume = -11.3 dB` |

The `copy_out` defect and its fix are stated accurately in `crates/capture/src/wgc.rs`;
its module doc previously said the fix had "never been re-run on Windows", which
understated the state and has since been corrected.

### What **failed** on that hardware — do not soften these

| Failure | Level | Number |
|---|---|---|
| Cannot sustain the configured frame rate at 4K | Verified on Windows hardware | **~24 fps** sustained against **30 fps** configured — issue #1 |
| Frames dropped by the encoder | Verified on Windows hardware | **~45% of delivered frames**; the debug line reads `dropped=1183` of ~3000 delivered — issue #1 |
| CPU cost | Verified on Windows hardware | **50.8% of one core** against a **< 5%** target — issue #1 (criterion 6 **FAIL**) |
| The media timeline does not track real time | Verified on Windows hardware — the measurement is unresolved there; **the mechanism is now measured on the dev host and fixed** | the divergence on the box is real, and two informal measurements disagree on direction: **≈0.81x** (media slower; segments written every ~1.25 s of wall clock — issue #2) versus **≈1.11x** (media faster; segment mtimes average **~899 ms** apart on the retained ring). Both measured the *write* position, which is what `span_ms` is; the mechanism is the frame-rate conversion (see below), and the fix takes the dev host's ratio from 0.660× to 0.987×. **The box's own number is still unmeasured** |
| RSS (the one criterion-6 half that passed) | Verified on Windows hardware | 235 MB, peak 267 MB against a < 400 MB target — issue #1 |

The box's two figures are **both readings of the write position** (`span_ms` is
`completed segments × segment_time`), which is why they can disagree between runs and why
neither settles the mechanism on its own: what they measure is how fast ffmpeg's muxer got
through a grid that required `R` frames a second — slower when the encoder was short, and
apparently faster in a run whose scratch cap had evicted segments and shifted the ledger's
origin. The earlier confident **0.81x** figure stays **withdrawn** (issue #2's correction
comment), and the *rerun that will settle the box's number* is prescribed below. On the dev
host the same reading is now 0.987× rather than 0.660× after the timeline fix, because the
grid that made it a throughput measurement is gone.

**How to measure it properly — do not infer it from file mtimes.** mtimes measure *writes*,
not the media they carry, so a cadence read off mtimes is not the media-vs-real-time ratio.
On the box, take the periodic debug line and compare its `span=` field against wall-clock
time across a sustained run, and its `fps=` field against the configured rate. The ratio that
matters is media-time (`span`) per wall-clock second, read from the log on a run long enough
(minutes) to average out segment jitter.

**What the dev host found, and what it fixed. The mechanism is the frame-rate conversion,
not the declared rate.** Measured here (ffmpeg 9.0.2, the real argument list) and corrected
twice on the way, so read this as the final account:

- With the segment muxer in place, a declared `-framerate R` makes ffmpeg resample the video
  onto a rigid `1/R` grid, and the *default* conversion mode fills that grid by
  **inventing frames**: a pipe fed 122 frames at ~24 fps with 30 declared encoded 180 frames,
  `dup=58 drop=0`. In the real pipeline the multiplication is what hurts — a 4K arm fed 39
  frames over 12.6 s encoded **1631** of them, `dup=1592`, every scratch segment reporting
  `avg_frame_rate=120/1`.
- ffmpeg's own `speed=` field then says where the time goes in that arm: **0.333×**, i.e. the
  muxer's write position advanced one second per three seconds of wall clock, because the
  encoder can only emit ~40 of the 120 frames a second the grid demands. The ring's
  `span_ms` is exactly that write position (`completed segments × segment_time`), so
  *that* — not a stretched container timeline — is the number that falls behind. Measured
  at the pipeline level below: **0.660×**.
- The container's own timestamps are **not** stretched: each frame keeps the arrival time it
  was given and the invented frames only pad the gaps between them. Verified by tagging every
  fed frame and reading its media timestamp back out of the encoded segments (49 distinct
  frames, 1085 output frames, media span 8001 ms against an arrival span of 9027 ms — slope
  0.94 against *write* times, the small deficit being ffmpeg's own read lag, not a warp). So
  the box's original complaint was about the ring's clock and the *staleness* it implies
  (footage at the trigger was seconds old), not about time-compressed pictures.

**The fix, and the A/B that justifies it.** `-fps_mode passthrough` on the video output
(`crates/encoder/src/ffmpeg.rs::video_output_args`) removes the conversion: each frame
reaches the muxer with its arrival timestamp, so the media clock *is* the wall clock
whatever rate the machine manages, and the encoder is no longer asked to emit `R` frames a
second of which most are invented. Same machine, same stub capture, same
`--dev-software-encoder`, 12 s of media requested, 4K output scaled from the stub
(`fps = 120`, `adapt_fps = false`) and the 720p control (`fps = 30`, adaptation on). The
ratio is the ring's own clock: `span=` between two status lines over their wall-clock
timestamps, over the window that excludes the ring's fill.

| arm | declared | achieved | dropped by the encoder's queue | media/wall | segments/wall-second |
|---|---|---|---|---|---|
| 4K output, **before** (`e051b60`+`411c32e`) | 120 fps | 3.3 fps | 2295 of 2454 | **0.660×** | 0.66 |
| 4K output, **after** (`-fps_mode passthrough`) | 120 fps | 31.7 fps | 1172 of 1801 | **0.987×** | 0.99 |
| 720p control, before | 30 fps | 29.6 fps | 0 | **0.999×** | 1.00 |
| 720p control, after | 30 fps | 29.7 fps | 0 | **0.986×** | 0.99 |

The same A/B at the ffmpeg level, with the child's exact argument list, feeding 8 fps against
120 declared at a 4K output: before, 39 frames became 1631 coded frames and the write
position ran at `speed=0.333×`; after, 97 frames became **exactly 97** coded frames (no
invention at all) and `speed=0.832×`, limited only by how long the feed itself lasted.

**What that change costs, measured, and it is not nothing.** A VFR segment holds the frames
that arrived while it was open, which is `segment_time` **minus up to one frame interval**:
measured 0.82–1.00 s of media per 1 s segment at ~40 fps delivered, and ~0.94 s at the
recorder test's 10 fps. Three consequences, all recorded rather than smoothed over:

1. **A clip can be short of its nominal window by one frame interval per segment it is
   spliced from** — measured 2821 ms for a 3000 ms request at 10 fps in
   `crates/recorder/src/tests.rs`, and 2756 ms of video for a 3000 ms request in the 4K CLI
   run. Before the fix such a clip was exactly nominal and its footage was **seconds old**
   (the `hotkey pressed: media=12000ms wall=18619ms` line is that staleness); now the
   footage is current and the file is a few frames short. The recorder test asserts the
   honest bound (one frame interval per segment) instead of the old one.
2. **`span_ms` over-estimates the footage on disk by the same one frame interval per
   segment**, since the ring counts `segment_time` per completed segment without probing it.
   At 30–60 fps that is 1–3%; at the recorder test's deliberately slow 10 fps it is 6%.
3. **Timestamps are resolved at `1/R`** (the demuxer's timebase comes from `-framerate`), so
   frames read within one tick of each other share a timestamp and occupy no time. Measured
   at 4K with bursts arriving from the pipe: 38 repeated timestamps among the 117 frames of
   the produced clip, and none in the 720p control. The frames are all there and decode; the
   duplicated ones are simply not displayed.

**Splicing and playback survive that — checked, not assumed.** `-reset_timestamps 1` still
gives every segment `start_time = 0`; the segmenter still cuts ~1 s segments (13 segments and
13 s of media in the after-4K arm); `localplay_media::edit::concat_lossless` — the exact
function `ClipSplicer` calls — still produces a clip that probes with **both** streams and
whose every coded frame decodes (117 frames in the 4K clip, 46/46 in the regression test).
One caveat: a frame that lands just across a segment boundary makes the concat demuxer's
per-file offset step *backwards* at that boundary — measured 0 in three of the four real clips
and, in a deliberately starved synthetic feed, a worst case of ~16–46 ms (well under one frame
interval at 30 fps). `crates/encoder/tests/timeline.rs` bounds it rather than pretending it is
absent.

**The throughput probe is not, and cannot be, the timeline guarantee.** The last two commits
before this one (`e051b60`, `411c32e`) measured the encode rate at startup and declared
`min(configured, measured)` to both the pacer and the encoder child. That does not make the
timeline correct on the machine that matters, and the numbers now say so plainly: the probe
measured **62 fps** and the live pipeline achieved **8** on the same host in the same
configuration (the probe measures the encode path only — raw frames to an encoder to `-f
null` — while the pipeline is WGC readback + copy + pipe + encode + segment muxing). On the
Windows box, where NVENC is cheap and the 33 MB-per-frame readback is the known bottleneck,
the probe's number is *more* optimistic. It remains as what it always should have been: a
diagnostic that reports the sustainable rate and a pacing aid that keeps capture from paying
for frames the encoder will drop.

`gh issue list --state all` shows both issues still **OPEN**, and both still need the box:

- issue #1 (throughput): unchanged by all of this. What the box must produce is the 4K soak's
  `fps=`/`dropped=`, and criterion 6's CPU number.
- issue #2 (the timeline): the *direction and magnitude* question above is now answered by
  mechanism and by the local A/B (the ring's clock was the write position and it ran at
  0.66×; it is now ~0.99×), and the fix is in the shipping argument list. **The box's own
  number is still unmeasured** — one soak with `RUST_LOG=debug` and the `span=`-vs-wall-clock
  reading prescribed above is what settles it, and it is also the only place the WGC readback
  can show whether passthrough leaves the machine able to encode the frames it actually
  captures.

---

## 2. The capture path, item by item

Source checked with `git log --oneline` and by reading each module's own doc comment.

| Path element | Level | Evidence |
|---|---|---|
| **WGC video capture** (`crates/capture/src/wgc.rs`) — starts, delivers frames, pulls via `TryGetNextFrame` | Verified on Windows hardware | ran in the session (issue #1); the module doc records "It was run on Windows 11 on a 4K/150%-scaled desktop" |
| **WGC `copy_out` correctness** (copy the captured texture to staging *before* `Map`) | Verified on Windows hardware | the first real Windows run produced **pure black frames** because `copy_out` mapped a fresh staging texture without `CopyResource`; the fix issues that `CopyResource` before the `Map` (`crates/capture/src/wgc.rs::copy_out`). A decoded frame from a retained segment now carries **216 distinct colours**, R/G/B means **16.9 / 25.3 / 42.0**, and an 80798-byte PNG **visually confirmed to show a real desktop** — real pixels, not black. This resolves the earlier in-tree contradiction (the `wgc.rs` module doc claiming the fix was never re-run has been corrected). |
| **WGC `discard_pending`** — close surplus frames without the GPU readback (`dd921b3`) | Type-checked only | `dd921b3` message: "NOT measured here: there is no Windows host" |
| **WASAPI loopback audio** (`crates/capture/src/wasapi.rs`) — endpoint-native format path | Verified on Windows hardware | the session produced clips with audio and criterion 8 passed (issue #1) |
| **WASAPI engine-side sample-rate conversion** (`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`, `0848fc7`) | Type-checked only | `0848fc7` message: "UNVERIFIED at runtime … nothing here has ever executed on Windows" |
| **The silence detector** for a converted-but-silent endpoint (`2a63949`) | Type-checked only | it only fires on the path above, which has never run; `crates/capture/src/lib.rs` says the failure mode is "external and **unverified at runtime**" |
| **Encoder child** (`crates/encoder/src/ffmpeg.rs`, ffmpeg sidecar) — H.264 via hardware encoder | Verified on Windows hardware | `h264_nvenc` in issue #2's arg list; criteria 4/5 passed |
| **Audio over loopback TCP** (`47cd974`) | Verified on Windows hardware | `tcp://127.0.0.1:51234` in issue #2's arg list |
| **Hardware-encoder selection + smoke test** (`6e63977`, `xtask probe`) | Verified on Windows hardware | the `advertised, FAILS` case was observed on the box (see §1) |
| **`Frame` move instead of clone on submit** (`f6a5a0e`) | Type-checked only | `f6a5a0e` message: "not measured here — there is no Windows host to measure it on" |
| **Ring buffer, segment ledger and cap eviction** (`crates/replay`) | Verified on Windows hardware | segments written; criterion 2 passed (issue #1), and the retained ring now records the cap (**15 MB**) and oldest-first eviction (§1) |
| **Clip splicing** (`crates/replay`, `crates/media`) — lossless `-c copy` remux | Verified on Windows hardware | the 13,091 ms clip; criteria 4 and 5 passed (issues #1, #2) |
| **The media-time trigger** (`55f1541`) | Verified on Windows hardware | fixed and confirmed on the box (issue #2: "Verified on hardware") |
| **The self-test trigger** — the same media-time trigger, reached without synthesising input (`d8ec96b`) | Verified on the dev host (the trigger), not on Windows | `apps/localplay-cli/tests/self_test_clip.rs::the_self_test_trigger_writes_a_clip_through_the_hotkey_path` drives the whole command in-process over real ffmpeg: it waited for the ring to hold the configured window, called `Recorder::clip_now()` once, and wrote a real clip (8,053 ms for an 8,000 ms window, `encoder=libx264`). What still needs Windows is not the trigger but the keypress that reaches the *same* call |

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
| Desktop **background half** — the tray menu and its state-dependent labels, the tooltip and icon state, the menu-action → shell-call mapping, the close-to-hide rule, the `[app]` defaults and the autostart decision table | Verified on the dev host | `apps/desktop/src-tauri/src/background.rs` unit tests (pure functions over plain data), `config.rs` tests for the new `[hotkeys]`/`[app]` reader, `apps/desktop/src/lib/recording.test.ts` for the two display rules, and the headless screenshot state `11-hotkey-not-installed`. **The tray, the window hide and the keypress themselves are not covered by any of this** — see §9. |
| The hotkey **trigger path** in the GUI — one press, one clip, through `RecorderHost::clip_now` | Verified on the dev host (headless) | `apps/desktop/src-tauri/src/commands.rs::one_hotkey_press_writes_exactly_one_clip_through_the_recorder` drives a **real** recording (stub capture source, libx264, a real ring, a real splice, a real index row) and asserts exactly one clip file, one index row and one count in the engine. The press is an in-process call — no input is synthesised. |
| **Hotkey registration failure is reported** (`225a1fe` in `crates/events`) | Type-checked only (Windows) + verified on the dev host (the parse half) | the `listen` path that carries `RegisterHotKey`'s error back and returns it — with the chord named — compiles for `x86_64-pc-windows-msvc` and **has never run**: registering a chord needs a Windows message queue. The parse-failure and "no global hotkey on this platform" branches are unit-tested (`background::install_hotkey`). |
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
| Self-test clip trigger (`--self-test-clip-after`) — the clip path without synthetic input | `d8ec96b` | Verified on the dev host (an integration test drives it end to end over real ffmpeg); never run on Windows |
| Desktop background half: tray, global clip hotkey, hide-on-close, `[app] start_with_system` | `225a1fe`, `e3fcb95`, `d20d748` | Verified on the dev host (headless: pure logic + a real recording driven through the trigger path). **Never observed on any machine**: no tray icon drawn, no window closed, no key pressed, no Run-key write executed |
| A hotkey registration failure is returned rather than swallowed (`crates/events`) | `225a1fe` | Type-checked for `x86_64-pc-windows-msvc` (`cargo check --target`); the CLI's half is verified on the dev host (`localplay-cli` tests still pass, the listener is a no-op off Windows) |
| `cargo xtask verify` — the one-command acceptance harness + its report | `07b90fc` | Verified on the dev host only, and only in the sense that it runs and reports honestly there: its clip/trigger/report/parsers/criterion-7 paths all executed, against the stub capture and the software encoder. **It has never run on Windows**, which is the only place it is meant to matter |
| The pipeline **paces capture to a rate it can actually keep**: a startup throughput probe (`encode.adapt_fps`, default on) measures the encoder at the real capture resolution, and `min(configured, measured)` is given to both the pacer and the encoder child — one number, read back out of the encoder's own `-framerate`. **This is a pacing aid and a diagnostic, not the timeline guarantee** (see the two rows below, and §1) | `e051b60`, `411c32e` | Verified on the dev host: the decision's arithmetic, the new config key, the probe's bounds (a real libx264 probe, a dead encoder, a wedged child), and the pacer/encoder agreement are all covered by tests, and the startup lines below came from real CLI runs. **The probe's accuracy at 4K on an NVIDIA GPU is unverified**: only libx264 (software, content-sensitive) was measurable here, its numbers do not transfer to NVENC (hardware, content-blind), and the probe measures the encode path only — on this host it reported 62 fps where the pipeline achieved 8 |
| The **media timeline is the frames' arrival timestamps**, not a declared grid: `-fps_mode passthrough` on the video output, so a machine that cannot keep up drops frames (held in the picture) instead of resampling the clock onto `1/R` — the fix for issue #2 | this change | Verified on the dev host end to end: a 4K pipeline that cannot keep up went from **0.660×** to **0.987×** media per wall second against a 0.999× control (table in §1), and the encoder stopped inventing frames (39 delivered → 1631 coded before; 97 → 97 after). A new regression test (`crates/encoder/tests/timeline.rs::a_starved_pipeline_still_keeps_the_media_timeline_on_the_wall_clock`) declares 4× the rate this machine measures and asserts the ratio stays in 0.85–1.15 with no invented frames; with the option removed it fails with `7243ms of video for 3.000694916s of wall clock (2.414x)` and 37972 coded frames for 984 submitted. **Not verified on Windows**: nothing has run there, and `-fps_mode` on the pinned sidecar is evidenced only by the option's own strings inside `binaries/ffmpeg.exe` |
| A **VFR segment still splices, probes and plays**: `-reset_timestamps 1` keeps every segment at `start_time = 0`, the segmenter still cuts ~1 s segments, and the product's own lossless concat (`localplay_media::edit::concat_lossless`) produces a clip with both streams and every coded frame decodable | this change | Verified on the dev host: `crates/encoder/tests/timeline.rs::the_vfr_segments_a_starved_pipeline_writes_still_splice_and_play` splices starved-pipeline segments and decodes 46 of 46 frames; the recorder's end-to-end clip test and the 4K CLI run both probe with two streams (117/117 frames decoded). The cost is measured and bounded in §1: a clip can be one frame interval per segment short of nominal, and a boundary frame can step the concatenated timeline back by ≤ ~50 ms |

The two startup shapes that change produced, from real `localplay-cli buffer` runs on the
dev host (stub capture, `--dev-software-encoder`, so the encoder is libx264 at the stub's
1280x720 — **these numbers are this host's and say nothing about the box's**):

```text
# encode.fps = 30, measured far above it: nothing is reduced
INFO localplay_recorder: encode rate: 326.8fps sustainable at 1280x720 with libx264 (491 frames over 1.5s); encode.fps = 30 is within that, so capture runs at the configured 30fps

# encode.fps = 120 against a 4K output: reduced to the measured rate
WARN localplay_recorder: encode.fps = 120 is not achievable at 1280x720 on this machine: 61.9fps sustainable at 1280x720 frames scaled to a 3840x2160 output with libx264 (95 frames over 1.5s). Capturing at 61fps instead — the same rate the encoder child is told — so that the capture is not paying a readback and a copy for frames the encoder will throw away. The media timeline does not depend on this rate: frames carry their arrival timestamps, so a clip covers the seconds it was captured over either way. The configured rate was not reached at this resolution: lower encode.output_size (e.g. "1920x1080") or encode.fps to a rate this machine holds, or set encode.adapt_fps = false to declare 120fps anyway and accept the frames that will be dropped and held in the picture.
```

---

## 5. What CI covers, and what it does **not**

CI (`.github/workflows/ci.yml`) runs on **`macos-latest` only** — the development host.
Check it yourself:

```console
$ gh run list --limit 10
```

What a green run proves:

- the whole Rust **workspace test suite** (with `localplay-encoder/test-encoders`) — 375
  tests as measured on the dev host at this pass, `cargo test --workspace --features
  localplay-encoder/test-encoders` (0 failed, 1 ignored);
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

- **The box's own media/wall ratio has never been measured with the fix in place.** The
  mechanism is settled and fixed on the dev host (§1: 0.660× → 0.987×, with the encoder
  inventing no frames), and the box's two informal readings (≈0.81x, ≈1.11x) were both
  readings of the *write position*, which is what `span_ms` is. What needs the box is one
  soak with `RUST_LOG=debug`: the log's `span=` field against wall clock (never file
  mtimes), which is also the only place the 33 MB-per-frame WGC readback can show whether
  the remaining shortfall is capture rather than encode.
- **A VFR clip can be one frame interval per segment short of its nominal window**, its
  `span_ms` over-estimates footage on disk by the same amount, and a frame landing across a
  segment boundary can step the concatenated timeline back by ≤ ~50 ms. All three are
  measured in §1; before the fix the clip was exactly nominal and its footage was seconds
  stale.
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
- **The desktop shell has never been a background application in practice.** The tray icon,
  its menu, the icon/tooltip state changes, the close-to-hide behaviour, the notification-like
  hint and the `Ctrl+F8` keypress are all **unobserved**: no window has been opened, no
  taskbar has drawn an icon and no key has been pressed. What exists is unit-tested logic
  (`background.rs`), a trigger path that really writes a clip when it is called
  (`commands.rs`), and Windows-target compilation of the registration code. §9 lists it item
  by item, because "the tray is tested" is the kind of sentence that gets read as "the tray
  works".
- **`[app] start_with_system` has never written a registry value.** The sync's decision table
  and the `reg.exe` argument lists are unit-tested, and the spawn is compiled for Windows;
  nothing has run. The suite deliberately does not exercise the Windows path, because that
  would write the real `HKCU\...\Run` of whoever ran the tests.
- **The harness is unproven on Windows.** `cargo xtask verify` (`07b90fc`) has run only on
  the development host, against the stub capture backend and the software encoder. Its
  logic is tested there (parsers, the report writer, the criterion-7 path, the trigger),
  and it marks every check it cannot perform instead of omitting it — but its Windows
  sampler (`GetProcessTimes` + `K32GetProcessMemoryInfo`, the runbook's own recipe), its
  interactive-session wrapper (`scripts/verify-in-interactive-session.ps1`) and everything
  it claims about a *real* capture path are **compiled, not run**. A not-performed row in
  its report is not a pass, and a pass on the stub path is not evidence about Windows.

---

## 8. The desktop shell's background half, item by item

Added by `225a1fe` (the shared hotkey), `e3fcb95` (the shell's tray, hotkey and hide-on-close
wiring) and `d20d748` (the window's copy of it). It exists because the GUI had **no hotkey at
all** and could not run without a window: the headline feature — press `Ctrl+F8`, get a clip —
worked in the CLI and not in the app, and closing the window ended the recording.

Every row below is what is actually established. **No window was opened, no taskbar drew an
icon, and no key was pressed while any of it was written**, and nothing here may synthesise
input (the trigger is always an in-process call).

| Item | Level | Evidence / what it would take |
|---|---|---|
| The chord is parsed once and normalised for display (`Ctrl+F8`) | Verified on the dev host | `crates/events/src/hotkey.rs` tests: parse, display round trip, a chord with no main key rejected |
| A `RegisterHotKey` failure is returned to the caller, naming the chord, instead of being swallowed | Type-checked only | `cargo check -p localplay-events --target x86_64-pc-windows-msvc`. Registering a chord needs a Windows message queue, so no test has ever produced either outcome on a real machine |
| The GUI installs the hotkey from `[hotkeys] clip` and a press takes a clip through `RecorderHost::clip_now` — the same call the button and the CLI make | Verified on the dev host | `background::install_hotkey` (the suite exercises the "no global hotkey off Windows" branch) and `commands.rs::one_hotkey_press_writes_exactly_one_clip_through_the_recorder`, which drives a **real** recording end to end and asserts exactly one clip file, one index row and one clip in the engine's counter |
| **The keypress itself** — that pressing the chord reaches that listener on a real machine | **Unverified** | Needs Windows and a keyboard. `RegisterHotKey` + `GetMessageW` is the CLI's own path (and the CLI's hotkey was exercised interactively in the 2026-09-23 session), but the desktop app's wiring of it has never run |
| The tray's state: icon choice, tooltip, and the labels that follow the setting | Verified on the dev host (as data) | `background.rs` tests over `TrayView` / `MenuAction`; the three icons are decoded by a test (`the_tray_icons_are_decodable_pngs_of_one_size`) |
| **The tray existing at all** — an icon on a taskbar, a menu that opens, a click that dispatches | **Unverified** | Needs a desktop. The wiring is Tauri's `TrayIconBuilder` and a menu built from `MenuAction::ALL`; the ids and the action → call mapping are tested, the rendering is not |
| Close hides the window and never quits, so a recording survives | Verified on the dev host (the rule) / **Unverified** (the behaviour) | `background::close_action()` is pinned by a test; that the window hides and the process lives needs a window |
| Recording continues while the window is hidden | **Unverified in the GUI**; structurally true | nothing in the recording path reads the window (the engine runs on its own threads and the window is a reader), and the CLI's headless runs are the same engine — but the GUI's own case needs a window to hide |
| Quit from the tray flushes the encoder first | Verified on the dev host (the order) | `dispatch(MenuAction::Quit)` → `stop_recording`, then `quit`, asserted against a recording fake |
| `[hotkeys]`/`[app]` config defaults (`Ctrl+F8`, `start_with_system = false`) and the example file carrying them | Verified on the dev host | `config.rs` tests, including one that reads `config.example.toml`'s text |
| The autostart decision (`enable` / `repair` / `remove` / `leave alone`) and the `reg.exe` argument shapes | Verified on the dev host | `background::sync_autostart` against a fake `reg.exe`, plus each of the three argv shapes |
| **The registry write itself** | **Unverified** | Needs Windows. The suite must not write a real `HKCU\...\Run` value, so `apply_autostart`'s Windows arm is compiled and never executed |
| The window's copy of all of this: the hotkey line, the config path, the close hint | Verified on the dev host (headless pixels) | `apps/desktop/src/lib/recording.test.ts`, and the screenshot states `09-recording` and `11-hotkey-not-installed` |

**The collision decision, stated once.** `RegisterHotKey` registers a chord system-wide, so
the CLI and the desktop app pointed at the same `config.toml` **cannot both hold it**. Nothing
coalesces them and nothing double-fires: the second one to start fails to register and is told
so — the app in its window, its tooltip and its log (and it stays usable, because the button
and the tray still work), the CLI by refusing to start at all (a buffer run whose hotkey does
nothing is not worth starting). The alternative — a lock file or an IPC handshake — would add
a second failure mode to a mechanism the OS already arbitrates.

---

## 9. What to run on Windows next

The smallest ordered set of runs that moves the most items from *type-checked* to
*verified*, from cheapest to most informative. Each is a single command plus an
observation; the Phase 1 runbook
(`docs/runbooks/phase-1-verification.md`) already contains the PASS/FAIL detail.

> **Most of this is now one command.** `cargo xtask verify` (`07b90fc`) runs steps 1–7 and
> writes `target\verify\report.md`: the probe, a bounded buffer run with the CPU/RSS
> window, the media-vs-real-time ratio from `span=`, the cap and eviction, the clip taken
> through the CLI's own `--self-test-clip-after` trigger (**no synthetic input anywhere**),
> the ffprobe of that clip, and criterion 7's fail-loud path. Run it from an SSH session
> with `scripts/verify-in-interactive-session.ps1`, which puts it in the logged-on user's
> desktop session through a one-shot scheduled task and cleans up afterwards.
>
> Two caveats, both load-bearing. First, **the harness has never run on Windows either** —
> it was developed and exercised on macOS against the stub backends, so a fresh run is
> checking the harness as much as the app: start with the report's `capture_started` row
> (a missing WGC line, or `1280x720` geometry, means the session had no desktop and the
> rest of the report is meaningless), then read the rows marked *not performed*. Second,
> **it does not cover step 8 (the GUI) or step 9 (a real game) at all**, nor a real
> keypress, nor lip-sync by ear; those stay manual.
>
> The steps below remain the source of truth — and every one of them is worth running by
> hand at least once, because a report's green cell is not the same thing as a person
> having watched the clip and heard the audio.

1. **`cargo xtask probe`** — confirms which hardware encoders this box can actually open.
   Cheap, no capture. Confirms the encoder half of the path.
2. **`cargo run --release -p localplay-cli -- buffer`** on the 4K display for ~40 s, then
   read the debug line. Look for: **`skipped=` non-zero** (the readback skip, `dd921b3`)
   and **`dropped=` falling to zero** with **`fps=` reaching the configured rate** — this is
   the *only* thing that can neither be tested nor type-checked for the readback skip. Also
   re-measures **criteria 1 and 6** (issue #1).
3. **Take one clip and `ffprobe` it** — re-confirms criteria 3/4/5/8 on the current
   build. Use the self-test trigger rather than a scripted keypress:
   `cargo run --release -p localplay-cli -- buffer --self-test-clip-after 45` (it calls the
   hotkey's own `clip_now()`; no synthetic input), then press `Ctrl+F8` once by hand, when
   you are at the keyboard, to confirm the global hotkey itself fires. (The black-frame
   defect is already resolved — see §2 — so this is a regression check, not the dispute it
   once was.)
4. **Repeat run 2 at 1080p.** issue #1 asks explicitly whether the criteria pass at 1080p;
   that separates "4K is hard" from "the pipeline is slow", and it is the measurement the
   acceptance thresholds were actually written for.
5. **Set a low `scratch_cap_bytes` and soak 5 minutes** — re-confirms criterion 2 eviction
   *on the current build*; a **15 MB** cap is already on record in §1 from the retained ring.
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
10. **Start the desktop app and press `Ctrl+F8` with the window hidden.** Start recording from
    the window, close the window (it must *hide*, and the tray icon must still be there),
    press `Ctrl+F8`, then reopen from the tray: one clip must appear in the list with the
    window never having been on screen. This is the headline behaviour and no test can reach
    it — see §8. Read the log line `the clip hotkey Ctrl+F8 is installed` first; if it says
    `NOT installed` instead, the chord was taken and the reason is in that line.
11. **Run the CLI and the app against the same `config.toml` at once.** Whichever starts
    second must report that the chord could not be registered — the app in its window and its
    tooltip, the CLI by refusing to buffer — and exactly one of them must be able to take a
    clip. A silent second success would mean the collision detection is not wired.
12. **Check the tray's own items**: show/hide the window, start and stop recording from the
    menu, save a clip from the menu, open the config file (it must reveal `config.toml` in
    Explorer, or its folder when the file does not exist), and quit — quitting while
    recording must flush the encoder (the clip written before the quit must be complete).
13. **Set `[app] start_with_system = true`, restart the app, and check `reg query
    HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v localplay`.** Then set it back
    to `false`, restart, and check the value is gone and nothing else in the Run key was
    touched. Nothing has ever written that value.
