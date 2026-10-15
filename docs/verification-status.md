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

> **The Windows-only surface has now run on Windows.** On 2026-09-24 the whole Rust workspace
> was built *and its test suite executed* on the box (Windows 11, RTX 3090): every crate's
> tests, the egress guards, and — in a real desktop session, with the owner's approval — a
> genuine capture run that wrote a clip. That session is §10. It is the first time the
> recording engine, the storage policy, the game-presence code, the WGC and WASAPI capture
> backends and the shipping hardware-encoder selection have *executed* rather than being
> type-checked for it. Doing it found three test defects — each fixed — and one thing that is
> still broken on Windows and is not a test's fault.
>
> What has still never run anywhere: the desktop **GUI** (no window has been opened on the
> box, and the shell's background half has never been a background application in practice),
> `cargo xtask verify`, the two game integrations **against a real game**, the microphone's
> silence detector *firing*, `[app] start_with_system` writing a registry value, and anything
> that would need a synthesised keypress — which this project will not do on a machine that
> runs an anti-cheat.
>
> The earlier measurement stands as the state of *that* build, and has since been measured again on
> later ones: the 2026-09-23 session (RTX 3090, 3840x2160 at 150% scaling, [issue #1]) proved the
> capture→encode→ring-buffer→splice path can run, and found that **the pipeline cannot sustain the
> configured frame rate at 4K and burns ~50% of a CPU core doing it** ([issue #1]). Both of those
> have since come back as passing, and both measurements are on the box: **§13** for the rate
> (`fps=56.9/60`, 8 dropped of 10,525) and **§15** for the core (3.1–4.0% of one core, on a path
> that stops the captured frame crossing to the CPU at all).

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
live clock divergence; §9 lists what stays manual and §10 records which parts of it the
2026-09-24 session got to by hand instead.

[issue #1]: https://github.com/FisiFla/better-outplayed/issues/1
[issue #2]: https://github.com/FisiFla/better-outplayed/issues/2

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
at *type-checked only* — carrying `RegisterHotKey`'s failure back to the caller. (**That last
one has since moved**, and the move is the 2026-09-24 session: see §10 — the branch does run,
it runs on the *first* SSH login, and it is now verified rather than type-checked.) The measured-rate change (§4) added one at *verified on
the dev host* — the rate decision, its new config key and the probe's own bounds are tested,
and the startup lines were produced by real runs — while the *accuracy* of its measurement at
4K on the target hardware was explicitly not claimed. The timeline fix (§1, §4) adds two more
at the same level — that the media clock is the frames' arrival timestamps (a local A/B, a
regression test that fails without it), and that the VFR output still splices, probes and
plays (a splice test that decodes every frame) — and *demotes* the measured-rate claim to a
pacing aid rather than a timeline guarantee: 20 → 23 Verified on the dev host.

**These counts are stale, and left stale on purpose.** The 2026-09-24 Windows session (§10)
moves several items and adds a whole class of them — the recording engine, the storage policy,
the game-presence code, the CLI and the bundled-SQLite store now *run* on Windows rather than
being type-checked for it, and the hotkey-failure and self-test-trigger rows move to *Verified
on Windows hardware*. The numbers above are therefore wrong **in the direction of
under-claiming**. They have not been adjusted by estimate: a tally is worth having only if it
was counted, and the per-item tables below are the source of truth — §10 is the evidence for
whatever moved, and the next pass should recount from those tables rather than from this
paragraph. Nothing after §10 has been folded in either: §11–§15 each add claims of their own
that these counts do not include at all, and §15's — the zero-copy path's — are *Verified on
Windows hardware*.

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
| CPU cost | Verified on Windows hardware | **50.8% of one core** against a **< 5%** target — issue #1 (criterion 6 **FAIL** on this path; **met since on the zero-copy path, §15**) |
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

`gh issue list --state all` then showed both issues **OPEN**, and both still needed the box. Both
have since had their readings taken and both are now **closed** — #1 and #2 — leaving #3 open:

- issue #1 (throughput): **closed.** The box produced what it asked for and more: `fps=56.9/60`
  with 8 dropped of 10,525 (§13), and criterion 6's CPU number, which the zero-copy path then met
  at 3.1–4.0% of one core with `capture_copy=0.6–0.7%` (§15).
- issue #2 (the timeline): **closed.** The *direction and magnitude* question above was answered by
  mechanism and by the local A/B (the ring's clock was the write position and it ran at 0.66×; it is
  now ~0.99×), and the fix is in the shipping argument list. **The box's own number has since been
  taken** — §13's `media=180266ms wall=180372ms` over 180 s, a drift of 0.06% — which is what
  settles it, and which also shows passthrough leaves the machine able to encode the frames it
  actually captures.

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
| **Hotkey registration failure is reported** (`225a1fe` in `crates/events`) | **Verified on Windows hardware** (2026-09-24, §10) | `RegisterHotKey` genuinely failed on the box — `This operation requires an interactive window station. (0x800705B3)`, over SSH — and the error returned to the caller **named the chord**: "the chord Ctrl+F8 could not be registered (RegisterHotKey failed: …)". The previous claim was that this "has never run"; it has, and the branch that was expected to be unreachable in practice turns out to be the one an SSH session always takes. A `#[cfg(windows)]` test now asserts the failure names the chord. The parse-failure and "no global hotkey on this platform" branches remain unit-tested (`background::install_hotkey`). |
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
| Self-test clip trigger (`--self-test-clip-after`) — the clip path without synthetic input | `d8ec96b` | **Verified on Windows hardware** (2026-09-24, §10): the shipping CLI ran it in a real desktop session and took a clip — `self-test: the ring holds 7000ms of media; taking the clip` → a 7083ms file, indexed. Also verified on the dev host |
| Desktop background half: tray, global clip hotkey, hide-on-close, `[app] start_with_system` | `225a1fe`, `e3fcb95`, `d20d748` | Verified on the dev host (headless: pure logic + a real recording driven through the trigger path). **Never observed on any machine**: no tray icon drawn, no window closed, no key pressed, no Run-key write executed |
| A hotkey registration failure is returned rather than swallowed (`crates/events`) | `225a1fe` | Type-checked for `x86_64-pc-windows-msvc` (`cargo check --target`); the CLI's half is verified on the dev host (`localplay-cli` tests still pass, the listener is a no-op off Windows) |
| `cargo xtask verify` — the one-command acceptance harness + its report | `07b90fc` | Verified on the dev host only, and only in the sense that it runs and reports honestly there: its clip/trigger/report/parsers/criterion-7 paths all executed, against the stub capture and the software encoder. **It has never run on Windows**, which is the only place it is meant to matter |
| The pipeline **paces capture to a rate it can actually keep**: a startup throughput probe (`encode.adapt_fps`, default on) measures the encoder at the real capture resolution, and `min(configured, measured)` is given to both the pacer and the encoder child — one number, read back out of the encoder's own `-framerate`. **This is a pacing aid and a diagnostic, not the timeline guarantee** (see the two rows below, and §1) | `e051b60`, `411c32e` | Verified on the dev host: the decision's arithmetic, the new config key, the probe's bounds (a real libx264 probe, a dead encoder, a wedged child), and the pacer/encoder agreement are all covered by tests, and the startup lines below came from real CLI runs. **The probe's accuracy at 4K on an NVIDIA GPU is unverified**: only libx264 (software, content-sensitive) was measurable here, its numbers do not transfer to NVENC (hardware, content-blind), and the probe measures the encode path only — on this host it reported 62 fps where the pipeline achieved 8 |
| The **media timeline is the frames' arrival timestamps**, not a declared grid: `-fps_mode passthrough` on the video output, so a machine that cannot keep up drops frames (held in the picture) instead of resampling the clock onto `1/R` — the fix for issue #2 | this change | Verified on the dev host end to end: a 4K pipeline that cannot keep up went from **0.660×** to **0.987×** media per wall second against a 0.999× control (table in §1), and the encoder stopped inventing frames (39 delivered → 1631 coded before; 97 → 97 after). A new regression test (`crates/encoder/tests/timeline.rs::a_starved_pipeline_still_keeps_the_media_timeline_on_the_wall_clock`) declares 4× the rate this machine measures and asserts the ratio stays in 0.85–1.15 with no invented frames; with the option removed it fails with `7243ms of video for 3.000694916s of wall clock (2.414x)` and 37972 coded frames for 984 submitted. **Partially verified on Windows** (2026-09-24, §10): the timeline suite ran on the box against the *system* ffmpeg 9.0.2 and the starved-pipeline test **passed** there, so `-fps_mode passthrough` behaves as intended on Windows too. What this does **not** cover is the pinned sidecar (`binaries/ffmpeg.exe`), which is still evidenced only by the option's own strings inside the binary; and one sibling assertion in that file — the audio stream's length against the video's — **failed on Windows**, and it turned out to be a real loss of audio rather than a test artefact: the encoder's own queue was too short to ride out a momentary stall in ffmpeg and discarded 760 ms of a 3 s feed (§10.4). |
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

> **CI is currently dark, and not because of the code.** The account's GitHub Actions minutes
> are exhausted, so a queued job never gets a runner: it fails in seconds with no steps and
> `runner_id=0`, which is the same signature as a broken workflow and is not one. Verified by
> re-running a commit that had passed in 3m28s unchanged — it failed the same way. Every `gh
> run list` since 2026-09-24 11:51 shows that. **So `gh run list` is not evidence about this
> tree right now**, and the gate below was reproduced by hand instead: the exact steps, run
> locally in order, at commit `a8a8b9b` (§11).

CI (`.github/workflows/ci.yml`) runs on **`macos-latest` only** — the development host.

What a green run proves:

- the whole Rust **workspace test suite** (with `localplay-encoder/test-encoders`) —
  `cargo test --workspace --features localplay-encoder/test-encoders`, **516 passed, 0 failed,
  1 ignored** at `a8a8b9b` (the count was 375 at the pass this section was written for, and it
  has grown with every phase since);
- **`cargo check` for `x86_64-pc-windows-msvc`** of the cross-checkable crates
  (`capture`, `encoder`, `events`, `replay`, `media`, `xtask`) — *type-checking*, not
  running. No MSVC linker is needed because `check` stops before linking;
- the desktop frontend: `npm ci`, `svelte-check`, `vitest`, production build; and then the
  desktop shell's own Rust tests;
- `cargo clippy` over the workspace (warnings surfaced, not fatal).

What a green run does **not** prove — quoted from the workflow's own header:

- **No Windows runtime verification.** "Not one line of the WASAPI or WGC backends is
  *executed* here; they are only compiled. A green run says nothing about how the app
  behaves on a real machine." (§10 is where they were executed — by hand, on the box.)
- **No GUI window.** "No window is opened, the screen is never captured and no input is
  synthesised."
- **`localplay-store`, `localplay-recorder`, `localplay-cli` and the desktop shell are not
  cross-checked for Windows from this host**, because rusqlite's bundled SQLite C source
  cannot be built for MSVC from macOS. They are host-verified *here*, and CI cannot carry
  that check — which is not the same as unverifiable: §10 records the whole workspace,
  bundled SQLite included, being built **and its tests run** on a Windows host. What that
  needs is a Windows runner or a human, and there is no Windows runner.

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

- ~~**The box's own media/wall ratio has never been measured with the fix in place.**~~
  **Measured on the box, 2026-09-25 (§13):** `media=180266ms wall=180372ms`, a drift of
  106 ms over 180 seconds. The media clock is the wall clock to 0.06%, so `pre_seconds`
  means real seconds. The mechanism (`-fps_mode passthrough`) was already in the tree; this
  was the one reading that was outstanding.
- **A VFR clip can be one frame interval per segment short of its nominal window**, its
  `span_ms` over-estimates footage on disk by the same amount, and a frame landing across a
  segment boundary can step the concatenated timeline back by ≤ ~50 ms. All three are
  measured in §1; before the fix the clip was exactly nominal and its footage was seconds
  stale.
- ~~**Criteria 1 and 6 fail on 4K hardware.**~~ **Re-measured on the box, 2026-09-25 (§13),
  and they now part company.** Criterion 1 **passes**: the pipeline sustains **`fps=56.9/60`**
  at 3840x2160 with `h264_nvenc` and drops 8 frames of 10,525 — against ~24 fps at 30 with 45%
  dropped when the issue was filed. Criterion 6 **failed on this path** — **88.9% of one core**
  for the same work at 2.4x the frame rate (1.49 cpu-percent per fps, against 2.1 before — better
  per frame, higher in total) — and the diagnosis was right: the cost is memory bandwidth, not
  arithmetic, 3840x2160x4 bytes at 57 fps being ~2.1 GB/s moved twice (GPU to staging, staging to
  the pipe), which is memcpy speed. ~~**No code change fixes that except moving fewer bytes**~~ —
  **the change that stopped moving them at all is the answer, and it shipped: §15 measures the
  zero-copy path at 3.1–4.0% of one core with `capture_copy=0.6–0.7%`, and it is the default.**
  The other route named here, capturing at the output size, was never needed: the WGC frame pool
  is created at the capture item's size, so it is not reachable from configuration. The criterion
  is left absolute, as the owner decided; §15 is the honest reading of it now.
- **The pacer already skips before the readback**, which both issues proposed as a fix: the
  pump consults the pacer first and only calls `next_frame` when a frame is due. On this
  display it has little left to win — `skipped=204` against `frames=10525` (§13) — because the
  panel delivers ~60 fps, not the 144 Hz the issue assumed, so almost no frame is read back
  and then thrown away.
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
- **`[audio] enabled = false` is honoured**, and this bullet is the last one here to be
  withdrawn. The game-audio track is no longer declared unconditionally: with `false` the
  encoder is given no audio input at all — no loopback listener is bound, `ffmpeg_args` emits
  neither the `tcp://` input nor `-c:a`, and no writer thread starts — so a clip and a session
  file carry one video stream and **no audio stream**. The capture side follows: `build_sources`
  never opens the WASAPI loopback, so a track nobody asked for is not even captured and thrown
  away. `Encoder::submit_audio` refuses a block at such an encoder rather than dropping it
  silently into nowhere, and the startup line says `video only: no audio track` rather than
  leaving it to be discovered in the file. One combination is refused rather than recorded —
  `enabled = false` with a microphone — by the recorder before anything is opened, and again by
  `FfmpegEncoder::spawn` as the backstop: a container's audio tracks are named by position, so
  `localplay_media::edit::audio_titles` would call a one-track recording of a voice
  `Game Audio`. The two keys beside it are fail-closed as before: `source` and `codec` each have
  one implemented value (`"loopback"`, `"aac"`) and anything else is refused by
  `Config::validate`, which names what *is* implemented rather than recording something else.
  `bitrate_kbps` still reaches ffmpeg's `-b:a` — 192 kbps for a front-end that carries no value
  (the desktop shell, which does not parse `[audio]`) — and is unused when `enabled = false`.
  **It is measured**: §17 below carries the box run — one `h264` stream, and no audio stream at
  all.

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
5. ~~Set a low `scratch_cap_bytes` and soak 5 minutes~~ — **no longer possible, and no longer a
   criterion.** Criterion 2 measured eviction from a bounded disk ring; a replay buffer holds its
   footage in RAM now and evicts against `ram_cap_bytes`, which the ring's own tests cover
   exhaustively. What remains checkable on real hardware is the RAM budget under a long soak:
   watch that the reported `bytes=` settles rather than climbing (§12).
6. **Force a `vendor` that is not present** — confirms criterion 7's fail-loud path on the
   current build.
7. **Set the Windows default playback device to 44.1 kHz or 96 kHz and record a clip** —
   exercises the WASAPI autoconversion (`0848fc7`) and the silence detector (`2a63949`).
   **Half done** (§10.2): the autoconversion *has* now run — the box's default render endpoint
   reported `native_sample_format=F32` and the backend logged `converting=true`, and the clip
   that came out had audio in it. What has still never run anywhere is the **detector
   firing**: that needs an endpoint that converts *and* is silent, which a live machine does
   not offer.
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

---

## 10. The 2026-09-24 Windows session — the suite, run on Windows

The first time this project's tests were **executed** on Windows rather than type-checked for
it. Method: `git archive HEAD | tar -xz` on the box (`C:\Users\player\localplay`; the box has
no `git`), then `cargo test`. The toolchain is cargo/rustc 1.98.1 and the *system* ffmpeg
9.0.2 — the same ffmpeg version as the dev host, which matters for reading §10.4 below.

### 10.1 What ran, and what it proved

`cargo test --workspace --features localplay-encoder/test-encoders --no-fail-fast`, in an SSH
session (session 0, no desktop):

| Crate | Result |
|---|---|
| `localplay-capture` | 27 passed |
| `localplay-encoder` | 22 + 3 + 2 passed; **1 failed** (§10.4) |
| `localplay-events` | 151 + 8 + 10 + 11 + 4 passed, plus doc-tests |
| `localplay-media` | 34 + 2 passed; **1 failed** (§10.3), now fixed |
| `localplay-recorder` | 58 passed |
| `localplay-replay` | 22 + 1 passed |
| `localplay-store` | 59 + 5 + 12 passed |
| `localplay-cli` | 18 + 1 + 2 + 2 + 3 passed; the capture test refused (§10.2) |
| `xtask` | 44 passed, 1 ignored |

Two things this settles that no amount of type-checking could:

* **rusqlite's bundled SQLite compiles and runs** on this toolchain, so the whole store and
  recorder suite executes on Windows.
* **The egress guards pass on Windows** — `only_the_loopback_modules_talk_to_a_socket_at_all`
  and `no_http_client_is_constructed_outside_the_loopback_module` — which is the platform whose
  network stack they exist to guard.

### 10.2 The real capture, in a real desktop session

Run under `schtasks /create … /it` so it executed in session 1 (`running as the console user in session 1`,
`explorer running (a real desktop): True`), with the **shipping** build — no test-encoder
features, so no CPU software fallback exists:

```
WASAPI loopback capture started on the default render endpoint
  native_sample_rate=48000 native_channels=2 native_sample_format=F32
  requested_sample_rate=48000 requested_channels=2 requested_sample_format=s16 converting=true
clip clip-1790250224: video 7000ms  audio 6997ms  drift 86ms
wrote …\clip-1790250224.mp4 (7083ms, 4782052 bytes, encoder=h264_nvenc)
indexed clip #1 (media t=2000ms, 7083ms, 4782052 bytes, h264_nvenc)
recorded bookmark event #1 at media t=7000ms against clip #Some(1)
buffer session #1 closed: 9 segment(s), 7114374 bytes in the scratch ring, no session file
→ ffprobe: 3 streams — video, audio, audio
```

Read that against the ledger above. It moves, in one run: **WGC capture** on a real display,
**WASAPI loopback** including the engine-side sample-rate conversion (`converting=true`),
**hardware-encoder selection in the shipping build** (`h264_nvenc` chosen by `vendor = "auto"`
on the box's own GPU — six `type-checked only` rows in §1 turn on this alone), the **microphone
track surviving the lossless concat** (two audio streams in the clip — the defect §10.3 fixed,
now confirmed on hardware), and the **bookmark event** a manual clip records, linked to its
session.

Two caveats, stated rather than glossed:

* **The clip's audio tracks were unnamed** — three streams came back as `video, audio, audio`
  with `stream_tags=name` **empty**, so a player showed two anonymous "Audio" tracks with
  nothing to say which was the game and which the microphone. **Fixed**: the cause was that a
  `-c copy` through the concat demuxer does not carry per-stream metadata, measured with and
  without `-map_metadata 0`, so the concat now re-applies the names it knows
  (`localplay_media::edit::audio_titles`, which the encoder also reads them from). The
  regression test is `crates/media/tests/lossless.rs::concat_keeps_the_audio_track_names_the_encoder_wrote`,
  and removing the fix makes it fail with `[None, None]`.
* **The pipeline could not hold 30fps at 4K** and said so itself:
  `only 28.0 frames per second are reaching it, and its queue is dropping the rest (2 in total)`
  — the engine's own warning, doing exactly what it is written to do. It is the issue #1
  finding reproduced on a *current* build, at 30fps rather than 60. **Still open.**

### 10.3 Three test defects, found and fixed

All three are the same species as the CI flakiness fixed earlier the same week — an assertion
that encodes a fact about the **host** rather than a property of the code — but across the
platform instead of across load. Each is committed with its reasoning.

| Test | What it asserted | What Windows did |
|---|---|---|
| `hotkey::tests::listening_off_windows_reports_no_failure_and_never_fires` | `listen(chord).expect(…)` — not gated to non-Windows, though its name and doc say that is what it is about | `listen` really tried `RegisterHotKey`, which failed (`0x800705B3`) — so the assertion was about the session, not the code. Now gated, with a Windows counterpart asserting the failure is *reported* and names the chord |
| `lol::client::tests::a_refused_connection…` | that a closed loopback port yields `FetchError::Connection` | port 1 **timed out** instead of being refused. The product was already right (`is_no_game()` accepts both); the test was over-specific about which the OS produces |
| `media::binaries::tests::explicit_path_wins` | the literal path `/custom/dir/ffmpeg` | `/custom/dir\ffmpeg.exe` — a real and legitimate difference, since a bare `ffmpeg` is not an executable there |

### 10.4 The Windows audio timeline: a queue too short for one stall (found 2026-09-24, fixed 2026-10-15)

`crates/encoder/tests/timeline.rs` failed on Windows on the assertion that the audio and video
streams agree:

```
46 frames over 3.000228s: 4 segments, video 2934ms, audio 2023ms, fed audio 144480 frames = 3010ms
the two timelines must agree to within the muxer's granularity: video 2934ms, audio 2023ms
```

Per segment, from the sibling test on the same box:

```
seg-000000.mp4: video 1000ms, audio 896ms
seg-000001.mp4: video 1000ms, audio 560ms
seg-000002.mp4: video 1000ms, audio   0ms
seg-000003.mp4: video   67ms, audio   0ms
```

The audio stopped about 1.5s into a 3s feed while 3010ms was *fed*. Two things were known, and
both ruled out the obvious answers: the box's ffmpeg is 9.0.2, the same version as the dev host,
so it was not an ffmpeg difference; and **the real capture run in §10.2 measured `video 7000ms
audio 6997ms`**, 3ms apart, so it was not general to the Windows product. The assertion was
**left failing rather than widened**, which was right — it was guarding a real property, and the
property was broken.

**The audio was really being lost, by the encoder's own queue.** The test feeds the pipeline for
three seconds of wall clock, so both sides of it can be counted. Instrumenting the feed loop
(temporary, reverted) showed the producer was innocent: 11.6M iterations of the loop, one 10 ms
audio block drained per iteration, never a burst, no iteration whose video wait exceeded 50 ms,
`dropped video frames 0`. The loss was on the consumer side, in the encoder's own bounded queue:
**76 dropped audio blocks**, 760 ms. Offsetting each drop against the clock put them in **one
window, 1.380-2.134s of the 3.000s feed** — a single ~0.75 s stall that begins just after the
first segment boundary — and the arithmetic closes:

```
3010 ms fed - 760 ms dropped by the queue - ~207 ms still in flight at the end = 2043 ms muxed
```

**The fix is that queue's slack.** `AUDIO_QUEUE_BLOCKS` was 32 blocks, 320 ms, sized for a child
that had *stopped reading* — with the drop path as the answer. That is the right bargain for a
permanently wedged encoder (a hole in the sound beats a capture loop blocked behind it) and the
wrong one for a pause the child comes out of on its own a second later: at 320 ms of slack, one
transient stall discarded a quarter of the recording, permanently. It is now **300 blocks, three
seconds, ~576 KB**, with the comment above it saying why — the video queue is four frames and
cannot bend (33.2 MB each), the audio queue is a rounding error beside it, and the microphone's
queue is sized the same way because it carries the same payload to the same child.

Verified on the box at `e4d4098` plus this one constant, unmodified test, three consecutive runs,
`test result: ok. 3 passed; 0 failed` each time:

```
46 frames (15fps nominal, 30fps configured) over 3.0005s: 3 segments, video 2901ms, audio 2803ms, fed audio 144480 frames = 3010ms
```

The gap between the streams is **98 ms** against the assertion's 700 ms, and it does not move
run to run: the failing bound now has ~600 ms of headroom rather than sitting on the edge. The
audio is 207 ms short of what was fed, which is the tail arriving after the last video frame that
no segment boundary can mux — the test's own comment predicts exactly that. The siblings on the
same runs: the starved pipeline `0 video + 0 audio dropped by the queues` with `media/wall =
0.984-0.985`, and the splice test `4 segments spliced into 320x180 3067ms of video (46 coded
frames), 3031ms of audio; 46 frames decoded; 0 repeated, 0 backwards`.

The stall itself has only ever been measured on the synthetic feed; §10.2's real capture did not
reproduce it. So this is measured insurance on a real path — the queue that carries loopback
audio is this queue — rather than the fix for a shipped symptom, and it is written down that way
on purpose. **The bound did not move. What was fixed was the behaviour it bounds.**

### 10.5 What this session did **not** run

The list is shorter than it was, and every remaining item needs something a script cannot
supply:

* **The desktop GUI.** No window was opened on the box. The recording-wiring claims move from
  *type-checked only* to nothing at all — they are still unexecuted, and §8 stands.
* **`cargo xtask verify`** (`07b90fc`): never run on Windows. §10 is not a substitute — it ran
  the *tests*, not the acceptance harness, and the harness's own report is still the thing to
  read.
* **A real game.** Both Phase 4 integrations remain unrun against one.
* **The silence detector *firing*** (`2a63949`): the converted-loopback path executed in §10.2
  (`converting=true`), but the detector did not trip — a live microphone was connected.
* **The pinned sidecar** (`binaries/ffmpeg.exe`): §10.2 used the box's *system* ffmpeg.
* **A synthesised keypress.** Never, by policy: input synthesis is exactly what must not happen
  on a machine running an anti-cheat, and a test is not a reason to make an exception. The
  hotkey's *failure* path is now verified (§10.3); its *success* path needs a human at the
  keyboard.

### 10.6 One consequence worth knowing

`localplay-cli` **exits** when the clip hotkey cannot be registered — deliberate, and the
comment explains why (a buffer run whose chord does nothing is not worth starting). The
2026-09-24 session showed what that means in practice: **the CLI cannot run at all over SSH,
as a service, or on a headless box**, and neither can `--mode session`, which records
continuously and needs no hotkey. Kept as designed, by the owner's decision; recorded here
because "the CLI refuses to start headlessly" is the kind of thing that belongs in a ledger
rather than in a bug report later.

### 10.7 The encoder's tests had not built on Windows for three weeks

Found on 2026-10-15, trying to run the whole crate after the §10.4 fix: **`cargo test -p
localplay-encoder` did not compile for `x86_64-pc-windows-msvc` at all.** Three errors, in two
test modules:

| site | what it was | introduced by |
|---|---|---|
| `crates/encoder/src/mft.rs:1149` | the test called `probe_hardware_encoders()`, a function that had grown a `VideoCodec` parameter | `3f2fe49` *Make the hardware path codec-aware, so HEVC can take it* |
| `crates/encoder/src/ffmpeg.rs:1890,1895` | `a_dead_writer_reports_ffmpegs_exit_status_and_stderr` used `std::os::unix::fs::PermissionsExt` and a `#!/bin/sh` stub with no `#[cfg(unix)]` gate | `6e63977` *smoke-test hardware encoders so an advertised-but-unusable one fails loudly* |

Neither was visible from anywhere that runs. The box's Windows sessions since then named a test
file (`--test timeline`), which builds only that target; and this repository's own Windows gate
is a `cargo check` of the *libraries*, which never compiles `#[cfg(test)]` code:

```
cargo check -p localplay-capture -p localplay-encoder -p localplay-events \
            -p localplay-replay -p localplay-media -p xtask --target x86_64-pc-windows-msvc
```

So the platform the product ships on could not run the crate's tests, and had not been able to
since 2026-09-25 — the date of both commits above, both after the 2026-09-24 Windows session that
had recorded the encoder's counts. It matters beyond tidiness: the §10.4 failure was read for
three weeks as "one assertion in the timeline target", and the timeline target was the only one
anybody *could* run there.

Both sites are fixed rather than deleted. The `mft.rs` test is the one that asks the machine what
hardware encoders it has, which is what `crates/encoder/src/mft.rs` exists for; the two
`ffmpeg.rs` tests are the dead-writer and bounded-shutdown defects, whose stand-in children are
POSIX scripts, so each gets the `#[cfg(unix)]` gate and a line saying why. The `ed8ce59` edit
that added the shutdown test had also stranded the dead-writer test's doc comment above the new
test, so the doc went back to the function it describes.

**And the gate now looks at the tests.** The CI cross-check step gained `--tests` and
`--features localplay-encoder/test-encoders` (the feature the workspace suite uses: without it
`EncodeConfig::for_tests_software` does not exist and `crates/encoder/tests/stream_output.rs`
cannot be checked either). Verified locally against `x86_64-pc-windows-msvc`: the whole
cross-check list now compiles with its tests, and the encoder's library-only check still reports
only its pre-existing unused-import warning. `localplay-media` and `localplay-events` already
gated their own unix-only test code properly, so the encoder was the whole of it.

On the box the whole crate is green and has run there for the first time since 2026-09-25:
`30 + 3 + 2 + 3 + 3 passed, 0 failed`, exit 0. That includes `mft.rs`'s own test, which is the
one this defect silenced — and it is the test that asks the machine what hardware encoders it
has, so its answer is worth recording: **ten MFTs**, of which the **NVIDIA H.264 and HEVC
encoders accept the D3D11 device manager** (`d3d11=true`) and the Intel Quick Sync ones, which
enumerate the same encoders, do not.

---

## 11. The CI gate, run by hand (2026-09-24, `a8a8b9b`)

CI's runner quota is exhausted (§5), so the workflow's steps were run locally, in the
workflow's own order and with its own `working-directory` for each — including the one that
runs from `apps/desktop/src-tauri` rather than the repo root, which is easy to get wrong and
silently runs the wrong tests if you do.

| CI step | Where CI runs it | Reproduced locally |
|---|---|---|
| Rust workspace tests | repo root | **516 passed, 0 failed, 1 ignored** |
| Windows cross-check (`capture`, `encoder`, `events`, `replay`, `media`, `xtask`) | repo root | `Finished` — all six type-check for `x86_64-pc-windows-msvc` |
| Frontend — `npm ci` | `apps/desktop` | clean install |
| Frontend — `svelte-check` | `apps/desktop` | **0 errors, 0 warnings** |
| Frontend — `vitest` | `apps/desktop` | **134 passed** |
| Frontend — production build | `apps/desktop` | `✓ built` |
| Desktop shell Rust tests | `apps/desktop/src-tauri` | **112 passed** |
| Clippy | repo root | only the pre-existing warnings: the `unwrap()`-in-tests lint and one `too many arguments (8/7)` in `EncodeConfig::hardware` |

What this is worth: the same gates, the same results, and **not** a substitute for CI in one
respect — it ran on the machine that wrote the code, so it is exactly as good as the dev host
is representative. The Windows cross-check is a `check`, not a run, either way. What it does
mean is that nothing in this tree is currently waiting on a gate nobody has exercised.

---

## 12. In-memory replay buffering — landed, and what the first attempt actually got wrong

A replay buffer that writes **nothing** to the SSD while it is only buffering, and touches the
disk only when a clip is saved. `-f segment` used to write a file per second into a scratch
directory and a filesystem ledger deleted the oldest; that write churn is gone in buffer mode.

### What landed, and what each piece is worth

| Piece | Where | Evidence level |
|---|---|---|
| `EncodeConfig::output` / `EncodeOutput::FragmentedStream`; `.stdout(Stdio::piped())`; `Encoder::take_output_stream()` | `crates/encoder/src/ffmpeg.rs` | **Verified on the dev host** — the element-for-element args test still passes, and 3 tests drive a real ffmpeg |
| The stream mode writes no files | `crates/encoder/tests/stream_output.rs` | **Verified** — `the_stream_mode_writes_nothing_to_disk`, monotonic fragment timestamps, and a remux into a playable clip |
| `FragmentSplitter` — fragmented MP4 → `Fragment { seq, start_ms, duration_ms, keyframe, bytes }` | `crates/media/src/fragments.rs` | **Verified on the dev host** — 5 tests against a real ffmpeg stream, including byte-at-a-time equivalence |
| `MemoryRingBuffer` — push, byte/duration eviction, window selection, `assemble` | `crates/replay/src/ram_buffer.rs` | **Verified on the dev host** — 8 tests |
| `ClipSplicer::splice_from_memory` / `splice_stream` — `-i pipe:0 -c copy`, audio titles re-applied | `crates/replay/src/splice.rs`, `tests/ram_clip.rs` | **Verified on the dev host** — a clip produced from a real stream, probeable, with both streams and the right names |
| The live-stream seam: the ring is parsed on the thread that owns the pipe, while frames are fed | `crates/replay/tests/ram_clip.rs` | **Verified** — `a_live_stream_is_ingested_as_it_arrives_without_stalling_the_pipeline` |
| The window contract: footage covering `[T - pre, T + post)` is selected whole | `crates/replay/tests/ram_clip.rs` | **Verified** — `a_window_inside_the_held_footage_covers_the_whole_request`, positive and negative |
| `Ledger::Memory` — buffer mode uses the in-memory ring; encoder built first because the ring is fed by its stdout | `crates/recorder/src/lib.rs`, `src/memory_ring.rs` | **Verified on the dev host** — the recorder suite runs buffer mode end to end |
| **Constraint 1**: buffering writes nothing, and a saved clip is the only thing that is written | `crates/recorder/src/tests.rs` | **Verified** — `buffering_writes_nothing_to_disk_and_only_a_saved_clip_does` |
| `buffer.ram_cap_bytes`, defaulting to 256 MiB, and the RAM budget that replaced the scratch cap as the engine's check | `crates/recorder/src/config.rs`, `config.example.toml` | **Verified on the dev host** — the default, the example, and a pre-existing config without the key |

Full workspace suite on the dev host: **535 passed, 0 failed, 1 ignored**. The desktop shell's
separate crate graph: **112 passed, 0 failed**. The Windows cross-check (`media`, `replay`,
`encoder`, `capture`, `events`) is clean, and the Windows *run* is its own section below. Clippy on
the touched crates adds no warning that was not already there.

### The thing the first attempt got wrong, because it is the useful part

The first attempt wired the recorder, saw a **3000ms clip request come back as 2000ms**, and was
reverted on the belief that the reader thread was stalling. Two things were wrong with that, and
both are worth keeping:

1. **There was no stall.** The status line read `fps: 9.99, configured_fps: 10,
   effective_fps: 10`, and `fps` is *measured from frames actually submitted* — positive evidence
   the pipeline ran at its configured rate. The two `frames` readings that looked like a freeze
   were ~200ms apart, not the two seconds the surrounding test wording suggested. A stall is a
   number you can read off the pacer, and this one was not there. The claim sat in this ledger as
   fact until the next pass re-read the evidence instead of its own summary of it.

2. **The short clip was a test defect, and the test was passing for the wrong reason.** The wait
   before the trigger was `s.frames > 0 && s.span_ms > 0` — "the buffer is non-empty" — while the
   assertion later in the same test needs `pre + post` = 3000ms of footage to exist. The file
   ledger's span **lags a whole segment behind** (its newest file is untrusted until a later one
   appears), so "the span has moved" meant two segments had been written: enough for the window by
   accident. A fragment in the in-memory ring carries its own length, so that ring's span is
   *exact*, the same wait returned after one segment, and the window resolved to
   `[0ms, 2000ms)` for a trigger at 1000ms with a 2000ms pre-roll. The clip was correct for the
   footage that existed; the footage did not exist yet.

   The fix was to the **wait**, not the assertion: ask for the window, which is what was meant and
   is right for both rings instead of relying on one of them being slow to count. The instrument
   that made this legible was four numbers logged at the splice — `trigger`, the window, the
   selected span and the ring's held range:

   ```
   clip from RAM: trigger=1000ms window=[0ms, 2000ms) selected=2000ms held=2000ms footage=[0ms, 2000ms]
   ```

   A pass had already been spent inferring this from surrounding test output. It is cheap to
   just say it.

### Two parse bugs found by measurement, not by reading

Both were in `sample_span_ticks`, which reads a fragment's own sample durations out of its `moof`
so the ring knows its length on arrival instead of borrowing the next fragment's start — the
borrowed rule cost a whole fragment of span, and a clip's window is resolved against that span.

* `tfhd`'s conditional fields are positional, so `default_sample_duration` sits at an offset that
  depends on which of `base_data_offset` and `sample_description_index` are present.
* `trun`'s per-sample entries are **4 to 16 bytes wide, not 4** — a duration may be followed by a
  size, flags and a composition offset. A fixed 4-byte stride sums those into the total, which is
  how a one-second fragment measured **3576ms**.

The file ledger's rule — *a segment's end is proved by the next segment's start* — is kept as the
fallback for a sample table this build cannot read, so the failure mode is the previous behaviour
rather than a guessed length.

### It runs on Windows (2026-09-25)

The tree was shipped to the box as an archive — it has no `git` — and extracted to a **fresh
directory** (`C:\Users\player\localplay-ram`), so nothing of the owner's was overwritten, then
built and tested there natively (`cargo 1.98.1`, host `x86_64-pc-windows-msvc`). League was not
running at the time and nothing captured, displayed or injected anything: `cargo test` is CPU
only.

**The buffer-mode tests pass on Windows**, which is the claim constraint 3 needs:

| Target | Result |
|---|---|
| `localplay-recorder --lib` | **60 passed, 0 failed** — reproduced **4 consecutive times** |
| `buffering_writes_nothing_to_disk_and_only_a_saved_clip_does` | **ok** — constraint 1, on Windows |
| `a_recorder_records_produces_a_clip_and_stops_cleanly` (buffer mode, RAM ring) | **ok** |
| `a_clip_records_why_it_was_taken` | **ok** |
| every `a_full_session_*` test | **ok** — session mode unchanged |

The whole-workspace run reported three failing targets. Two of them **also fail at `1875a89`**, the
commit before this feature existed, which was checked rather than assumed by extracting that
revision to a second directory and running the same three targets:

| Target | at `1875a89` | at the RAM commit |
|---|---|---|
| `localplay-cli --test self_test_clip` | **FAILED** | FAILED — pre-existing (this is §10.6's hotkey defect) |
| `localplay-encoder --test timeline` | **FAILED** | FAILED — pre-existing |
| `localplay-recorder --lib` | ok. 58 passed | 1 failure in the workspace-wide run, **4/4 clean** in its own suite |

The one recorder failure was `a_watched_game_starting_and_stopping_drives_a_full_session_and_the_retention_pass`,
asserting the status had dropped its game after the session ended. It is **not** a regression from
this change, and the evidence is two measurements rather than a reading of the diff: it passes
**6/6 in isolation** and the recorder suite passes **4/4** with it included. It is a presence-watcher
timing assertion that is sensitive to how much else is running — and the workspace-wide run has the
whole tree's binaries in flight at once. This change adds a reader thread and two tests, so it
genuinely does alter the load profile, which is enough to tip a test like that without being a
defect in it. Worth fixing as its own piece of work; not evidence about the RAM buffer.

**Fixed, and it was a defect in the code rather than noise in the test.** The status kept the
game until *after* the join and the retention pass that follow a watched game's stop, while the
engine clears `running` as its own shutdown returns. `running` was therefore false with `game`
still set for as long as that work took, which is why load widened the window enough to see it
rather than being the cause. `spawn_supervisor` now clears the game *before* the shutdown, so
`running == false` implies no game — which is what `RecordingStatusDto::matched_game` already
documents ("`None` while nothing is being recorded"), and what makes the assertion deterministic
instead of a window. The test is unchanged, deliberately: it passes because the ordering it
asserts is now the ordering there is. Reproduced on the dev host by widening the window on
purpose — 600 extra expired sessions for the retention pass to evict — and polling every 200µs:
`game: Some("Dota 2")` alongside `running: false` before the change, `None` at that same instant
after it. Not re-run on Windows; the box is where the window was first seen, and this is the
dev-host diagnosis of it.

What is **not** claimed: nothing here exercises NVENC, AMF or QSV — the tests run the software
encoder, as they do on the dev host. The hardware-encoder selection was verified on this box in
§10, before this change, and this change does not touch it. A real capture run into a RAM buffer
on Windows is still unperformed; what ran is the test suite with stub sources.

### The decision, and what it cost: the file ledger is gone

The open question above was answered **by removing the type**, and the reasoning is worth keeping
because the alternative was defensible. Buffer mode taking RAM unconditionally — which is what "no
SSD writes while buffering" means — left `RingBuffer` reachable from nowhere. Keeping it as a
second implementation would have meant adding a config key to select it, and a capability nothing
selects is not really kept while its tests keep passing and its code keeps looking load-bearing.

What it provided that the other two modes do not: a replay buffer whose footage survives a crash.
That is genuinely redundant here rather than lost. A crash-survivable *session* is session mode,
which already exists; a crash-survivable *buffer* protects the seconds before a keypress that, by
hypothesis, never happened. The task asked for RAM buffering and forbade nothing, so this is a
judgement rather than a requirement — and it is reversible: `git log` has the type, its tests, and
this section explaining them.

The cost was not one file. Removing it meant something for every test and comment that named it,
and **three of those were tests whose subject was not the ring at all**:

| What named it | What was actually being tested | What happened |
|---|---|---|
| `apps/localplay-cli/tests/post_roll.rs` | the post-roll wait — the §10 bug where the wait starved ffmpeg | **retargeted** at the in-memory ring. Still three tests, now over the shipping path |
| `apps/localplay-cli/tests/clip_index.rs` | the index — splice, index, evict | **retargeted**. The ring was only its clip producer |
| `crates/replay/tests/end_to_end_clip.rs` | the file ring, end to end | **deleted** with its subject |
| `crates/replay/src/buffer.rs` | the ring and its ledger numbering | **deleted**; `BufferConfig` and `BufferStats` stay, both still used by the recorder |

Splitting it that way is the point worth recording: the deletion was not one decision but four, and
two of them were places where the file ring had been standing in for shared machinery. The suite
went from 538 to 535 passing — three tests fewer, each removed with the thing it tested.

**One consequence that was nearly missed.** The file ring was the only thing that *enforced*
`[buffer].scratch_cap_bytes`: it evicted to that cap on every scan, and `session.rs` deliberately
never does. With the ring gone the key had no reader at all — it bounded nothing. The note in this
section said the opposite when it was first written, that it "bounds a directory that session mode
writes into"; that was wrong, and it was corrected rather than left as a plausible sentence.

The key is now **removed** — from `BufferSection`, from `BufferConfig`, from `config.example.toml`
and from every fixture that carried it. Removing it is backward compatible: the config structs do
not `deny_unknown_fields`, so a `config.toml` still carrying the key keeps parsing and the key is
ignored, exactly as it was when it did nothing. What bounds disk usage in both modes is the
`[storage]` policy, which evicts whole clips and sessions.

Removing it also dissolved a test rather than deleting one. `a_full_session_records_into_its_own_
directory_and_ignores_the_scratch_cap` stated its subject through the cap — set to 1 KiB so that
enforcing it would visibly gut the session. With no cap to ignore, the property it was standing in
for is stated directly and the test is now
`..._and_keeps_every_segment`, asserting that four seconds of 1s segments are all still there. That
is the real difference between a session and a rolling buffer, and it is better said than implied.

---

## 13. The 4K soak on the box (2026-09-25) — the measurement issues #1 and #2 were waiting for

Issue #2's own closing comment says the one thing that would close it is "the box's own ratio, taken
with the fix in place" and that no number from the box had been taken. Issue #1 says the same about
`dropped=` / `fps=` / `configured=`. Both were taken in a single run.

### How it was run, so it can be repeated

`schtasks /create … /it` so the CLI ran in **session 1** (`Logon Mode: Interactive only`, with
`explorer` running), from the **release** build of the current tree, against
`%LOCALAPPDATA%\localplay\config.toml` — a copy of the shipped `config.example.toml`, so the fps,
output size and window are what a fresh install would use. `RUST_LOG=debug`, 180 s of capture, and
the CLI's own `--self-test-clip-after 180` to end it. A running League client or game aborts the run
before it starts: this captures the screen, which is not done while a protected game is up. Measured
**3840x2160** throughout, `h264_nvenc`, `WASAPI` loopback for audio.

### The numbers

| Reading | Issue #1/#2's measurement | This run |
|---|---|---|
| Sustained rate | ~24 fps against 30 configured | **`fps=56.9/60`** — 95% of a rate that is 2x higher |
| Dropped frames | 1183, ~45% | **8 of 10,525** — 0.076% |
| `media=` vs `wall=` at the trigger | divergence of unknown direction | **`media=180266ms wall=180372ms (drift 106ms)`** — 0.06% over 180 s |
| Post-roll reached | every press timed out before the trigger fix | **`span=185416ms covers 185266ms`**, 5133 ms after the press |
| Clip | — | **36165 ms, 84,897,902 bytes**, video+audio, splice drift **49 ms**, indexed |
| The probe's accuracy at 4K with NVENC | "**unverified**" | **`82.2fps sustainable at 3840x2160 with h264_nvenc`** |
| CPU | 50.8% of one core at ~24 fps | **88.9% of one core at ~57 fps** |
| RSS | 235 MB peak 267 MB | **202 MB peak 271 MB** |

### What this closes, and what it does not

**Closed: the timeline half of issue #2.** The media clock is the wall clock to 0.06% over three
minutes, so `pre_seconds` now means real seconds. That was the outstanding question; the mechanism
(`-fps_mode passthrough`) was already in the tree and is not what this run tests.

**Closed: issue #1's headline and its drop counter.** "4K capture cannot sustain 30 fps" is no
longer true — it sustains **57 of a declared 60** at 3840x2160, and drops 0.076% rather than 45%.
The issue's suggested fix for the wasted readback is also **already implemented**: `pump_once`
consults the pacer *first* and only calls `next_frame` when a frame is due, calling
`discard_pending` otherwise. This run shows why that change has little left to win here —
`skipped=204` against `frames=10525`, because this display delivers ~60 fps and not the 144 Hz the
issue assumed, so there are almost no frames being read back and thrown away.

**Not closed at this point: criterion 6** (< 5% of one core) — **closed since, by the zero-copy path
(§15)**; this paragraph is left as written because it is the honest reading of *this* path, the
ffmpeg-sidecar one. It is *worse* in absolute terms than the issue's figure — 88.9% against 50.8% —
because the run is doing 2.4x the frames per second. Per frame it is better (1.49 against 2.1
cpu-percent per fps), which is what the timeline fix should look like: the same work, more of it, and
no longer thrown away. The cost is memory bandwidth, not CPU arithmetic: 3840x2160x4 bytes at 57 fps
is ~2.1 GB/s moved twice (GPU to staging, staging to the pipe), which is memcpy speed. **No code
change fixes that except not moving the frame**, and there are only two ways to do that: encode on
the GPU (the design spec's Phase 2) or capture fewer pixels. **The first is what §15 then did** —
measured at **4.0% of one core with `capture_copy=0.7%` and 59.6/60 fps**, now the default wherever
a hardware encoder is present. The second turned out to be unnecessary as well as unreachable: the
WGC frame pool is created at the capture item's size, so it is not a configuration away, and §15 is
the measurement that makes it moot rather than merely unexercised.

**Not measured: 1080p**, and the reason is worth recording rather than glossing. Issue #1 asks for it
"to separate '4K is hard' from 'the pipeline is slow'", and on this box it cannot be had by
configuration: only the primary monitor is captured, that monitor is 4K, and the pool size follows
it. Taken from session 0 the display list reports a 1024x768 virtual device, so the real geometry is
only visible inside the interactive session. A 1080p measurement needs either the display resolution
changed (invasive) or the scaled capture above (a change).

### And an independent confirmation, for free

The same run exercised the in-memory ring on real Windows hardware for the first time — §12 listed
that as unperformed. The run's own closing line:

```
buffer session #2 closed: 34 segment(s), 84335007 bytes held in RAM, no session file and nothing written while buffering
```

84 MB held in RAM, and the only file written in 180 seconds is the clip that was asked for. The log
line that reports the held bytes had been printing `bytes=0` for exactly this case, because it read
the *disk* figure from `Ledger::stats`; the status field and the log now read the same
`buffered_bytes` value once, so they cannot disagree.

---

## 14. Where the 4K CPU actually goes (2026-09-25) — `capture=97.1% submit=0.1%`

Issue #3's remaining half is the CPU figure, and the decision it needs — whether to move the frame
out of system memory — turns on which half of the cost the readback is. A process-wide CPU
percentage cannot say: it cannot distinguish the 33.2 MB GPU-to-CPU copy per frame from time spent
waiting for ffmpeg. So the pump now measures its own two expensive calls
(`PumpCounts::capture` / `::submit`) and the periodic line reports both as a share of wall clock.

Same box, same display, same 4K/60 soak as §13, with the attribution:

```
frames=8788 segments=34 bytes=93230279 span=155649ms dropped=9 dropped_audio=0 dropped_mic=0
  skipped=193 fps=56.7/60 configured=60 capture=97.1% submit=0.1%
```

| | |
|---|---|
| Inside the capture backend (`next_frame` + `discard_pending`) | **97.1%** of wall clock |
| Inside the encoder's submit, i.e. waiting for ffmpeg | **0.1%** |

**The cost is the readback, and ffmpeg is not blocking us at all.** That is a stronger statement
than §13 could make: the 88.9%-of-one-core figure is almost entirely the copy out of the GPU, and
the encoder's own work is invisible in this loop because it happens on the other side of a queue
that never fills.

The consequence for issue #3 is that **97% of the current cost is addressable by removing the
readback alone** — the target of "under 3%" is credible rather than aspirational — and that a
change which only *reduces* the bytes (capturing at the output size, 4x fewer) leaves roughly a
quarter of the wall clock in the same place. It also settles a question §13 left open: the encoder
is not where the money is, so a native encoder that still required a CPU copy would buy almost
nothing.

What this does **not** say: this is the readback path, the shipping design, and every number above
belongs to it. It *bounds* what removing the readback can save — 97% of the loop, so the target of
"under 3%" is credible rather than aspirational — and it does so from the inside of the loop rather
than from a total. **The path it bounds has since been built and measured, and the prediction is
what came back: see §15**, where the copy falls to 0.7% of the loop and the core to 4.0%.

---

## 15. The readback is gone: the zero-copy path, and HEVC on it (2026-09-25)

§14 bounded what removing the readback could buy and could not test it, because the path did not
exist. It exists now, it is in the tree, and it is the **default wherever a hardware encoder is
present**. This is the measurement §14 asked for. It was taken on the same box, against the same
4K/60 soak and the same example config, by the work recorded in
`docs/plans/2026-09-25-localplay-tier-2-zero-copy.md` and `docs/plans/2026-09-25-localplay-hevc.md`.

What it is: the WGC texture goes to NVIDIA's H.264 Media Foundation encoder unchanged (`ARGB32`,
no colour conversion), the encoder's own thread turns it into an H.264 elementary stream, and
ffmpeg **copies** that stream instead of encoding one. No pixel of any frame crosses to the CPU,
which is the entire point — §14 measured that crossing as 97% of the loop. `encode.zero_copy` is
an `Option<bool>`: unset means *use the hardware path when the hardware is there*, `true` means
*require it and refuse to start without it*, `false` turns it off.

### The numbers, same box, same soak

| | shipping path | hybrid, inline | hybrid, threaded |
|---|---|---|---|
| `capture_copy=` | 69.6% | 0.7% | **0.7%** |
| `CPU` | 88.9% of one core *(§13)* | 3.1% | **4.0%** |
| `fps=` | 55.5–56.9/60 | 35.0/60 | **59.6/60** |
| `dropped=` | 8 | 2288 | **0** |
| exits on its own | yes | **no** | **yes** |

The threaded run's own closing numbers:

```
CPU 4.0% of one core, RSS 278.9 MB
EXITED after 36.2s of watching
no ffmpeg left behind
frames=5710 segments=72 ... dropped=0 ... fps=59.6/60 capture=93.1% submit=0.2%
capture_wait=61.3% capture_copy=0.7%
buffer session #8 closed: 72 segment(s), 178668538 bytes held in RAM
CLIENT_EXIT=0
```

**Criterion 6 passes: 4.0% of one core against a `< 5%` target.** **Criterion 1 passes on this path
too, and more strongly than §13**: 59.6 of 60 with **zero** dropped frames, against 56.9 and 8.

Read the three columns in order, because the middle one is really two states. The inline hybrid's
first soak *never exited* — its process was still alive eight minutes after its log stopped, blocked
rather than spinning (4.6 s of CPU in ten minutes) — and that shutdown defect was found and fixed
(the three causes are below) before the threading work. Repaired, the inline run exits on its own at
**CPU 3.1%**, still at 35 fps. So the threading change is what took the rate from 35.0 to 59.6 and
the drops from 2288 to 0, and it is *also* what raised the CPU from 3.1% to 4.0% — an extra thread is
not free. The trade is worth naming rather than letting 3.1 next to 4.0 look like noise.

### The count that matters is the default's

Verified with `config.example.toml` **copied and unmodified**, which is the case a new install gets:

```
zero_copy in the config as copied:  (unset - which is the default being tested)
CPU 3.6% of one core
EXITED after 36.2s
zero-copy: captured textures go straight to the hardware encoder, and ffmpeg copies the H.264
  instead of encoding it (chosen by default, because a hardware encoder is present)
the hardware encoder is open on its own thread encoder=NVIDIA H.264 Encoder MFT format=ARGB32
  size=(3840, 2160) adapter=NVIDIA GeForce RTX 3090
CLIENT_EXIT=0
```

The device on a null adapter is whichever adapter DXGI calls the default, and on a machine with an
integrated GPU that is not safe to assume — so it was read rather than assumed: adapter 0 is the RTX
3090 (the Intel UHD is present and is not it, and the QSV MFTs report `d3d11=false` and cannot take
the device at all). `MftEncoder::open_for_texture` takes the device *from the captured texture*, so
the encoder and capture cannot disagree about which GPU they are on.

### The shutdown, found on the way

Three ways to wait for ever were on that path at once, and the shutdown took whichever came first:

1. **The MFT's tail, sent with a blocking `send`** — written into a block whose own comment called
   it best-effort, but the only thread that can drain that queue is the one that may be blocked
   writing to a child that has stopped reading. Now `try_send`, and a tail that does not fit is
   *reported* rather than sat on.
2. **The order.** The writers were joined *before* the child was waited for, and a writer only ends
   when its pipe breaks, which only happens when ffmpeg reads or dies — so a child that never exits
   took the whole shutdown with it, silently, with the logger already stopped. The child is now
   waited for first, under a 5 s `SHUTDOWN_BUDGET`, and joined after.
3. **`std::process::Child::wait` has no timeout.** `wait_bounded` polls `try_wait` and kills at the
   deadline, and a status that arrives only because of that kill is reported as such.

Those three were fixed and the story still was not over, because the same mistake was one line
further up. The MFT's **per-frame** handover was a blocking `send` too, and that is the one the box
keeps hitting: five of six zero-copy runs on a later soak, two of eight after that, and two of four
in a probe built to ask. Every one of them was *after* the clip was written and indexed, so the
recording was never at risk — only the exit was, which is why it read as a soak that would not end
rather than as a clip that was wrong. Instrumented (`MFT-DIAG`) and read back, the thread was inside
that send when the join was reached, with its reader — the thread that writes to ffmpeg — stalled
behind a child that had stopped reading; and the step that could break that loose, `wait_bounded`
killing ffmpeg, sits *below* the join that was waiting. It is the tail's bug one nesting level in,
with the same consequence and none of the credit, because the tail is sent once and this is sent
once a frame.

The handover now has a deadline of its own, `HANDOVER_BUDGET` — 2 s, the budget the encoder calls in
that thread already use, because it answers the same question. It waits for room by polling
`try_send`, stops waiting the moment shutdown is asked for, and drops and counts the frame it could
not queue: `N encoded frames were dropped instead of queued: the video queue stayed full for 2s,
which is the ffmpeg writer thread not keeping up`. Four runs of the shipped hardware path, four
clean exits at ~22 s each, with that warning firing in exactly the run the old build hung on.
`c5cd703`.

The regression test asserts on **time** rather than output, because the defect was an absence, and it
is mutation-checked: raise the budget to an hour and it hangs. Suite at the time: 541, from 540
(546 in the workspace since, after later work).

### HEVC on the same path

`codec = "hevc"` with `zero_copy` left unset took the hardware path too:

```
CPU 4.2% of one core, RSS 356.9 MB
capture=94.6%  submit=0.1%  capture_wait=62.0%  capture_copy=0.6%
buffer session #13 closed: 35 segment(s), 167671496 bytes held in RAM
CLIENT_EXIT=0        no ffmpeg left behind
clip: codec_name=hevc  codec_tag_string=hvc1  3840x2160
```

`capture_copy=0.6%` is the number that says the pixels still never came back to the CPU. **It is not
a CPU or GPU saving, and reporting it as one would be wrong**: the saving came from deleting the
readback, which is codec-independent, and NVENC's HEVC pass costs slightly *more* per frame than its
H.264 one. HEVC buys quality per bit. Three things it does not yet match or do, recorded here rather
than discovered later:

- **Throughput.** 56.9 fps and 9 dropped, against the H.264 hybrid's 59.6 and 0, with the recorder
  saying so out loud (`the encoder cannot sustain the 60fps it was told`). Not a regression from
  zero-copy — `capture_copy` and the CPU are what that change moved — but the predicted cost of the
  codec, and the reason the default stays H.264.
- **~~The bitrate is ignored.~~ Fixed 2026-10-15, and it was never HEVC-specific.** The clip came out
  at 37 Mbps against a `bitrate_kbps = 20000` setting, and this section read that as HEVC-side rate
  control because the H.264 path "honours the same setting". That was an artifact of the two numbers
  being the same one: the path was told a hardcoded 20 Mbit/s and never received `bitrate_kbps`, so at
  the default 20000 an inert path and a working one look identical. Eight interleaved cases at 4K60,
  one run per build, separate them — h264 at 4000 **23,218 kbps → 4,979**, hevc at 4000 **22,261 →
  8,151**, while the ffmpeg pairs (3,935 and 17,635 before, 4,096 and 19,554 after) were already
  right. What is left is smaller and honest: the MFT aims near the setting rather than pinning it
  (1.2× at 4000 for h264, 2.0× for hevc, 1.6–1.8× at 20000) where the ffmpeg path lands within 3%,
  because `MF_MT_AVG_BITRATE` is a mean for the encoder's rate control rather than a ceiling. Making
  it exact wants ICodecAPI's rate-control mode, which is a further measurement and has not been
  taken. See `7971dc7`.
- **~~A diagnostic that hides the interesting answer.~~ Fixed 2026-10-15.** `probe_available_types`
  used to `continue` past any encoder whose output type it could not set, and that is exactly the
  encoder whose *answer* was the point: `GetOutputAvailableType` says which type it would have taken
  instead, and an MFT offers that whether or not it accepted the caller's. The refusal is now a row
  of its own, carrying the call's error, and its available output types are enumerated anyway — the
  input side is not asked, because an async MFT will not talk about its inputs until an output type
  has been accepted. Measured on the box, old code against new, same build, same throwaway caller, at
  a size every encoder has to refuse (`probe_available_types(Hevc, (16384, 16384))`): the old report
  names the eight Intel MFTs (which cannot be activated at all) and **both NVIDIA encoders are simply
  absent from it**; the new one names them, their refusals — `Invalid type. (0xC00D36BD)` for H.264,
  `The input type is not supported for D3D device. (0xC00D6D76)` for HEVC, the same error the
  ordering probe reports — and what each would have taken: exactly two output types, one under the
  friendly subtype and one under the vendor's private GUID (`5D0CE9DD-…` for H.264, the four-CC
  `'HEVC'` GUID for the HEVC MFT), each carrying a profile and the sentinel level `0xFFFFFFFF`, and
  **no frame size, no rate and no bitrate**. That is an encoder answering with its own defaults, and
  it is the answer the pipeline's type — 4K at level 5.1 — was being refused against. The refusal
  itself is the size/state puzzle the size-ceiling probe documents (the same type is accepted on a
  ladder that *rises* to 4K rather than starting there) and is **not** what this change fixed: what
  changed is that the diagnostic now reports it instead of hiding it.

### What this does not say

- **The CPU comparison spans two sessions — and the matched pair has now been taken.** 88.9% is §13's
  earlier soak; the hybrid's 3.1% and the shipping path's *split* were taken minutes apart on the same
  build with the same instruments, but the shipping path's CPU total was never re-taken in the session
  that produced the hybrid's (the shutdown hang ended it early). The like-for-like evidence for the
  *split* is still the split instrumentation. On 2026-10-15 the totals were paired: one run of the
  released CLI per path, in one interactive session, same display, same 4K/60 capture, same
  `--self-test-clip-after 45`, sampled once a second over both processes — `[encode] zero_copy = false`
  against `true`:

  | | `zero_copy = false` (readback) | `zero_copy = true` (hybrid) |
  |---|---|---|
  | `localplay-cli` CPU | 119.37% of one core | **4.18%** |
  | its `ffmpeg` child | 156.10% | 4.07% |
  | both processes | **275.47%** | **8.25%** |
  | CLI peak RSS | 363 MB | 327 MB |
  | `fps=` | 58.5/60 | 58.8/60 |
  | `dropped=` | 50 | 4 |
  | `capture_copy=` | 73.2% | 0.7% |
  | media written | 34 segments, 86,140,600 B | 72 segments, 180,706,473 B |

  The readback path spent ~2.75 cores of CPU to write 86 MB; the hybrid spent ~0.08 cores to write
  181 MB, over windows of the same length (43.0 s and 44.5 s of steady state). **That is worse for the
  shipping path than §13's 88.9% alone suggested, and the difference is the instrument**: §13 and
  `xtask verify` read the `localplay-cli` process, and at 4K/60 with the loopback track live the
  copy out of VRAM costs the CLI more than a core *and* hands ~1.5 cores of encode to a child that
  number does not contain. The hybrid is the only path whose encoder runs inside the process being
  measured (the MFT is in-process; the readback path encodes in the `ffmpeg` child), so a CLI-only
  figure flatters it exactly where the pair above is honest: the same sampler over **both**
  processes, 275.47% against 8.25%.
- **The spread is real.** 3.1%, 3.6% and 4.0% are three runs of the same path. Criterion 6 passes
  with room to spare in all three, but the figure is a run, not a constant.
- **`capture_wait=` is not a back-pressure signal.** It reads 61.1% while the inline encoder
  throttled the pump to 35 fps and 61.3% while a threaded encoder kept up at 59.6 — the same number
  for opposite situations. Most of it is a correctly paced 60 Hz capture idling between frames.
- **AV1 is not reachable on this hardware.** The 3090 does H.264 and HEVC through NVENC; AV1 is
  40-series and later.
- **1080p is still unmeasured**, for the reason §1 already gives: the pool follows the capture item,
  only the primary 4K monitor is captured, and no configuration reaches a smaller size.
- **Windows only, and by construction.** Off Windows there is no encoder MFT, so the hardware path
  cannot be selected: the default resolves to the ordinary path and logs why. That is also why the
  key is an `Option<bool>` and not a `bool` defaulting to true — an absent key resolving to `true`
  would make every machine without an encoder MFT refuse to start, and would have taken the test
  suite with it, since the suite runs on a Mac.

---

## 16. A clip's duration, and a CI failure on a commit that changed no code (2026-10-15)

`091d956` — a **documentation-only** commit — failed the "Desktop shell Rust tests" job on

```
commands::tests::one_hotkey_press_writes_exactly_one_clip_through_the_recorder
clip duration 4393ms is not the configured window (3000ms ± the segment grid)
```

Nothing in that commit could have changed a clip, so the assertion was measuring something
other than the code. It was measuring two things, and both are worth having written down.

**The container's duration is not the window's.** `MediaInfo::duration_ms` — and the DTO the
UI shows — is ffprobe's `format.duration`, the same number criterion 4 is verified against.
For a clip it is larger than either stream in the file. Measured on the dev host, fourteen
runs of that test, plain and with sixteen competing processes:

| window | video stream | audio stream | container |
|---|---|---|---|
| 3000ms | 3000ms (13 runs), 3100ms (1 run) | 3008ms | 3187-3396ms |

The **video stream is the window**: 3000ms, or 3100ms when the window's edge falls inside a
slightly longer fragment. The container carries the audio track's per-fragment padding and the
muxer's stitched timestamps on top of it, and that surplus is small and steady on a real feed:
the box's clip in §1 and §10.2 is **13,091ms against a 13,000ms target — 91ms over thirteen
segments**, about 7ms each, comfortably inside criterion 4's ±0.5s. The test's own clips come
out ~190ms over three fragments instead, which is a property of the synthetic stub feed or of
how the synthetic audio is muxed; the two feeds were not isolated against each other, so treat
the difference as unattributed rather than as a defect in either.

**The splice cuts on the fragment grid.** A clip takes the whole fragments overlapping
`[trigger−pre, trigger+post]`, so it overhangs the window by whatever the grid quantises to at
each end — the §7 note that "it can run up to one segment past the request" is this — and on a
loaded host the fragments themselves run past `segment_time`, because the forced keyframe
lands on the first frame *after* the boundary. That is host pacing, not splice arithmetic, and
it is the part that cannot be bounded by a millisecond constant.

The assertion now pins what the splice promises rather than a millisecond fit: the clip's
**video** carries both sides of the configured window (the pre-roll-only clip of ≈2s, and the
empty one, are the failures it rules out), and the overhang is at most one fragment at each
end. The container's duration is deliberately no longer asserted against the window: at a
3000ms window it is not 3000ms even on an idle host, so the honest options were a bound fitted
to the observations or no container bound at all, and §10.3's mistake is the first. The 4393ms
reading was not reproduced here (nine further runs under sixteen competing processes, all
3187-3396ms), and the failing run's per-stream durations were not captured, so which of the two
effects moved furthest on the CI runner is **unattributed**.

---

## 17. A video-only recording: `[audio] enabled = false`, honoured (2026-10-15)

`config.example.toml`'s `[audio] enabled` was the last key in that section the pipeline could
not act on: setting it to `false` produced a warning at startup and a recording that carried
the game audio anyway. The engine declared one audio input in its argument list, bound one
loopback listener and started one writer thread, and no configuration could remove any of the
three.

It can now. `EncodeConfig::game_audio` is the switch, and `false` means the encoder is given
**no audio input at all**: `ffmpeg_args` emits neither the `tcp://127.0.0.1:{port}` input nor
the `-c:a aac -b:a Nk` block, `FfmpegEncoder::spawn` binds no listener and starts no
`ffmpeg-audio-in` thread, and the container carries one video stream and no audio stream.
`Encoder::submit_audio` **refuses** a block at such an encoder rather than dropping it
silently — the disposition `submit_mic_audio` already took for a missing microphone — and the
argument-list test pins the *absence* of the input, the codec and the rate, because a flag left
in a list that nothing dials is not an error ffmpeg reports.

The capture side follows the same switch: `build_sources` creates the WASAPI loopback backend
only when the game audio is on, so a video-only recording does not capture a track it is not
going to write — what it saves is the capture as well as the encode. The pump takes the audio
backend as an `Option<&mut dyn AudioBackend>`, the shape the microphone already had, so "this
recording has no such input" is one absence rule for both instead of two.

One combination is refused rather than recorded: `enabled = false` together with
`[mic] enabled = true`. A container's audio tracks are named by position and
`localplay_media::edit::audio_titles` calls the first one `Game Audio`, so a one-track
recording of a voice would be labelled as the game's own audio — a file that says one thing and
holds another, which is the class of defect this ledger exists to catch. It is refused in
`Recorder::start_with_options` before anything is opened, and again in `FfmpegEncoder::spawn`
as the backstop for a front-end that reaches the encoder without passing through the recorder.

How far the key reaches: the CLI carries it (`audio_enabled: Some(audio.enabled)`) and no
longer warns about it; the desktop shell carries `audio_enabled: None`, which means the game
audio is recorded, because that shell does not parse `[audio]`; and a video-only run says so in
its own startup line (`capture geometry …, video only: no audio track`) rather than leaving it
to be found in the file. `bitrate_kbps` still reaches ffmpeg's `-b:a` and is simply unused when
`enabled` is false.

**Evidence.** The argument list, the listeners and the backends are the code's half of this; the
file's half is a run of the released CLI on the box at `92e5a70`, against the shipped example with
`[audio] enabled = false` (the key the run set is `enabled = false`, at line 134 of the
configuration the CLI actually read). Its own `--self-test-clip-after` ended the run after 45 s
with exit code 0, and:

- the startup line said it out loud — `capture geometry 3840x2160 at 60fps (frame counter starts
  at 0), video only: no audio track`, exactly one such line in the debug log;
- the clip it wrote, `clip-1792082573.mp4`, is **174,455,585 bytes** and `ffprobe` reports one
  stream, `0,h264,video`. **There is no audio stream in the container at all** — not an empty
  one, not a silent one.

---

## 18. HEVC on the hybrid: the throughput gap does not reproduce, and both codecs overshoot (2026-10-15)

§15 recorded a gap between the two codecs on the zero-copy hybrid — HEVC at 56.9 fps with 9
frames dropped against H.264's 59.6 and 0 — and left it open, because the two figures came from
different runs. On 2026-10-15 they were paired: one run of the released CLI per codec, in one
interactive session, same display, same 4K/60 capture, same `--self-test-clip-after 45`, both
with `[encode] zero_copy = true` and nothing else different between them.

| | H.264 (`h264_nvenc`) | HEVC (`hevc_nvenc`) |
|---|---|---|
| the probe's own ceiling | 74.2 fps | 77.7 fps |
| `fps=` | 58.8/60 | 59.0/60 |
| `dropped=` | 3 | 10 |
| `skipped=` | 14 | 17 |
| frames / media | 2979 / 50,322 ms | 3016 / 51,006 ms |
| bytes written | 181,128,764 | 165,825,260 |
| the clip's `bit_rate` | 39,564,433 — **1.98x** the 20 Mbps asked for | 35,747,208 — **1.79x** |
| the clip's `avg_frame_rate` | 59.3 fps | 58.6 fps |
| frames that never reached the encoder | 3 | 10 |

**The rate gap does not reproduce.** In one session on this box, HEVC holds the same frame rate
as H.264 — 59.0 against 58.8, both inside the spread §15 already published. What does reproduce
is a smaller and more specific difference: HEVC sheds about three times as many frames at the
handover (10 against 3, out of ~3000), and both codecs report the same warning — "its queue is
two deep because capture's handover ring is three, and the encoder is slower than the frame rate
it is being fed". So the mechanism behind the earlier 9-dropped figure is the handover rather
than one codec being unable to reach 60 fps, and the media timeline is intact in both runs
(50,322 ms and 51,006 ms of frames for ~51 s of capture) — which is what dropping at the
handover means: the frames are refused at the ring, not lost from the recording.

**What the pair does show plainly is the bitrate.** Both codecs were asked for 20,000 kbps and
wrote far more of it — H.264 39,564,433 bit/s (1.98x), HEVC 35,747,208 (1.79x) — so the rate
control is a suggestion the encoder aims near rather than a number it holds. That is the
`ICodecAPI` workstream's motivating measurement, and it is what §15's "tracks the setting with
1.2-2.0x overshoot" looks like as a paired number rather than a range.

---

## 19. The encoder accepts the rate and does not obey it (2026-10-15)

§18 left the overshoot as "a suggestion the encoder aims near", and the obvious way to make it a
number it holds is `ICodecAPI` — the transform's own control surface rather than the
`MF_MT_AVG_BITRATE` attribute on the output type. `crates/encoder/src/mft.rs` (commit
`f4dbf10`) casts the transform to `ICodecAPI` and sets `CODECAPI_AVEncCommonRateControlMode` to
CBR and `CODECAPI_AVEncCommonMeanBitRate` to `encode.bitrate_kbps * 1000`, logging the
encoder's answer either way and saying so out loud when a transform has no `ICodecAPI` at all.

The encoder answers yes, and the file does not move:

| H.264, `encode.bitrate_kbps = 20000` | the clip's `bit_rate` | against the setting |
|---|---|---|
| before any `ICodecAPI` call (§18) | 39,564,433 | 1.98x |
| `ICodecAPI` set **after** `SetOutputType` (`f4dbf10`) | 39,314,725 | 1.97x |
| `ICodecAPI` set **before** `SetOutputType`, plus a maximum | 39,124,625 | 1.96x |

Both calls return `Ok` — the debug log carries `the encoder accepted rate control mode (CBR) = 0`
and `the encoder accepted mean bitrate (bit/s) = 20000000`, twice per run (the warm-up output
type and the real one) — and the third row set `CODECAPI_AVEncCommonMaxBitRate` as well as the
mean, which moved the file by half a percent. So on this stack (Windows 11, RTX 3090, the NVENC
H.264 MFT) the property is advertised, accepted and ignored, and the order of the calls against
`SetOutputType` is not the reason, because moving them earlier changed nothing.

What ships is the first of the three, `f4dbf10`: the encoder is asked through its own interface
and its answer is logged. It does not claim to have pinned anything, because the measurement says
it has not. `MF_MT_AVG_BITRATE` still describes what the stream is.

**Not tried, and the honest edge of this thread:** a rate-control mode other than CBR (an encoder
that ignores CBR may obey VBR or a quality mode), `CODECAPI_AVEncCommonBufferSize`, and a
driver-side preset. None was measured, and each would be another experiment for a number a reader
of a clip's `ffprobe` output can already see is not being held.
