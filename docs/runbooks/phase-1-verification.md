# Phase 1 verification runbook

**Status: unexecuted.** This document tells a human how to decide, honestly, whether
Phase 1 works. As of writing, **none of it has been run on Windows.** The Windows
capture and audio backends only type-check (`cargo check --target
x86_64-pc-windows-msvc` from macOS); no frame and no audio sample has ever been
captured. A green `cargo test` on macOS exercises the synthetic stubs and proves none
of the eight criteria below — see [Known gaps](#known-gaps).

You need:

- the repository checked out on a **Windows** machine,
- a Rust toolchain (`cargo`) on `PATH`,
- a GPU that offers a hardware H.264/HEVC encoder (NVIDIA **NVENC**, Intel **Quick
  Sync**, or AMD **AMF**), and
- `ffmpeg` and `ffprobe` reachable. The simplest setup is both on `PATH`. Otherwise
  put them in a `binaries\` directory next to the built executable
  (`target\release\binaries\`), which is where the CLI looks second.

You do **not** need to have read the design spec. Where a result depends on your
machine (resolution, fps, CPU %, RSS), this runbook says what to measure and the
threshold to compare against; it never hands you a number to expect.

The eight criteria are the Phase 1 success criteria. Run them in order. Every one gives
(1) the exact command, (2) the exact observation that means **PASS**, and (3) what
**FAILURE** looks like — so "it did not work" is distinguishable from "it worked and I
misread it".

Conventions used below:

- Log lines are timestamped and printed to **stdout** by default.
- Lines shown as `key=value` are structured tracing fields (e.g.
  `width=1920 height=1080`); the values are the numbers you record.
- `%LOCALAPPDATA%\localplay\` is the app data directory: `config.toml`, `scratch\`, and
  `clips\` live there unless the config overrides `scratch_dir` / `clips_dir`.

---

## Before you start

### 0.1 Build a release binary

```powershell
cargo build --release --bin localplay-cli
```

Release matters for criterion 6 — a debug build's CPU and memory are not representative
of steady state. Use `--release` for every run below.

### 0.2 Find out which hardware encoders this machine has

```powershell
cargo xtask probe
```

If your checkout does not have the `xtask` cargo alias, the equivalent is:

```powershell
cargo run -p xtask -- probe
```

This runs `ffmpeg -encoders` and prints one line per candidate id — `h264_nvenc`,
`hevc_nvenc`, `h264_qsv`, `hevc_qsv`, `h264_amf`, `hevc_amf` — each marked `listed`
or `absent`, followed by a note that a 1-frame smoke test is still required.

- Note every vendor whose encoder is `listed`. That is the value you put in
  `encode.vendor` in step 0.3. On an AMD-only machine expect the `*_amf` lines
  `listed` and the NVENC/QSV lines `absent`.
- `probe` needs `ffmpeg` **on `PATH`** (it does not search the sidecar directory the way
  the CLI does).
- `listed` means ffmpeg advertises the encoder. It does **not** prove the encoder can
  actually open a session on your driver — the 1-frame smoke test that would prove
  that is deferred to Phase 2. Treat `listed` as necessary but not sufficient.

### 0.3 Configure

If `%LOCALAPPDATA%\localplay\config.toml` does not exist, the CLI runs with the built-in
example defaults (`pre_seconds = 30`, `post_seconds = 5`, `fps = 60`,
`segment_time = 1`, `scratch_cap_bytes = 2147483648`, `vendor = "auto"`). To change
anything, copy `config.example.toml` there and edit it. At minimum:

- `encode.vendor` — the vendor from step 0.2, or `"auto"`.
- `encode.fps`, `buffer.pre_seconds`, `buffer.post_seconds`, `buffer.segment_time`,
  `buffer.scratch_cap_bytes` — as you want them for the runs below.

`localplay` never reads configuration from environment variables.

### 0.4 Turn on the debug log

Criterion 2 is read from a line emitted only at `debug` level:

```powershell
$env:RUST_LOG = "debug"            # PowerShell
# or, in cmd.exe:  set RUST_LOG=debug
```

For the ledger line alone (much less noise) use `$env:RUST_LOG = "localplay_cli=debug"`.

To keep a transcript, run everything through a file. For example:

```powershell
cargo run --release -p localplay-cli -- buffer 2>&1 | Tee-Object -FilePath run.log
```

---

## Criterion 1 — Captures the primary monitor via WGC at the configured fps

**Command**

```powershell
$env:RUST_LOG = "debug"
cargo run --release -p localplay-cli -- buffer
```

Leave it running. This is also the process you use for criteria 2 and 3–6.

**Expected observation (PASS)**

At startup, on stdout:

- `capturing the primary monitor display="<name>"` (the WGC capture item's display
  name).
- `WGC capture started on the primary monitor width=<W> height=<H>` — `<W>x<H>` must
  equal your primary monitor's native resolution (Settings → System → Display).
- `rawvideo -s=<W>x<H> (capture native), encode output <W>x<H>` — the raw-video pipe
  size must equal that same `<W>x<H>`.
- `encoding with <encoder_name>` — e.g. `encoding with h264_nvenc`.
- `buffering <pre>s pre / <post>s post at <fps>fps; press Ctrl+F8 to clip` — `<fps>`
  must equal your configured `encode.fps`.

Within a few seconds, `seg-000000.mp4`, `seg-000001.mp4`, … appear in
`%LOCALAPPDATA%\localplay\scratch\`.

With `RUST_LOG=debug`, every ~200 ms you get a line:

```
frames=<n> segments=<n> bytes=<b> span=<ms>ms
```

`frames=` is a literal counter of the video frames actually submitted to the encoder
since startup — not a proxy. At startup the CLI also prints a one-line geometry summary,
`capture geometry <W>x<H> at <fps>fps (frame counter starts at 0)`, so the resolution
being captured is visible up front.

Over a run of `T` seconds:

- `frames=` should grow by roughly `T × <fps>` (as fast as the capture-and-encode path
  sustains; if it lags the encoder is the bottleneck, not the counter).
- `span=` should grow by roughly `1000` ms per second of wall clock.
- `segments=` should advance by one every `segment_time` seconds.

That is the rate check: `frames` counts the frames, `span` tracks the capture timeline,
and the `WGC … width=<W> height=<H>` and `rawvideo -s=<W>x<H>` lines confirm the
resolution and the pipe size.

**FAILURE looks like**

- Startup aborts with `Error: …` before any capture line — e.g. WGC could not start, or
  programmatic-capture access was denied. No `WGC capture started` line.
- `WGC capture started on the primary monitor` appears but `width=`/`height=` is **not**
  your monitor's resolution (e.g. `1280x720` would mean the synthetic stub ran, i.e. the
  real backend was not selected on this build).
- After 30 s, `frames=` is still `0`, `span=` is still `0`, `segments=` is still `0`, and
  no `seg-*.mp4` files exist — frames are not flowing.
- `frames=` grows materially slower than `T × <fps>` (e.g. well under half of `<fps>`),
  or `span=` grows materially slower than 1000 ms per wall-clock second — capture is not
  keeping up at the configured fps (or the pipeline is stalling).

**Do not misread:** if `segments=` has stopped climbing but `span` still grows, that is
normal ring behaviour once the scratch cap is reached (oldest segments are evicted) — not
a criterion-1 failure.

---

## Criterion 2 — Scratch directory stays at or under `scratch_cap_bytes`

**Command**

Run the same `buffer` process (with `RUST_LOG=debug`) for a **5-minute soak**, and read
the `bytes=` field from the debug ledger line.

To make eviction actually happen inside 5 minutes, set `buffer.scratch_cap_bytes` lower
than the soak will produce but comfortably higher than `(pre_seconds + post_seconds)`
worth of footage (otherwise the pre-roll window is truncated). Estimate one second of
video as `bitrate_kbps / 8` kilobytes plus `audio.bitrate_kbps / 8` kilobytes of audio,
then multiply by 300 s. For example, at `bitrate_kbps = 20000` that is roughly 2.5 MB/s,
so 5 minutes is roughly 750 MB — set the cap below that. Make sure the cap is also
larger than a **single** segment, because the ring always retains at least one segment
and the CLI aborts if it cannot satisfy the cap (see below).

**Expected observation (PASS)**

- The debug line's `bytes=` rises, then plateaus once the ring is full, and **never
  exceeds** the configured `scratch_cap_bytes` for the whole soak.
- The process keeps running — it never aborts.

**Measure the LEDGER, not the directory.** `bytes=` is `BufferStats.bytes_on_disk` — the
sum of the *indexed* segments. Do **not** run `du` / `dir /s` on the scratch directory
and compare that. The directory can legitimately hold **one extra segment** beyond the
ledger: `scanner::newly_complete` withholds the newest segment, because ffmpeg may still
be appending to it, so that file is not yet indexed — and therefore not yet evictable.
The ledger is the binding constraint.

**FAILURE looks like**

- The process aborts with
  `Error: scratch cap violated: <b> bytes on disk exceeds <cap>`. That is the code's own
  assertion on the ledger, so seeing it is unambiguous.
- The `bytes=` field ever exceeds the cap while the process keeps running — enforcement
  is broken.
- The scratch directory measures **two or more segments** over the cap, or far more than
  one segment over. (One extra file is expected and is not a failure; the ledger is what
  you compare.)

**Do not misread:** a single extra `seg-*.mp4` in the directory, above the ledger total,
is the withheld in-progress segment — normal, not a defect. Also, if you set the cap
smaller than one segment, the ledger can never satisfy it and the run aborts by design;
that is a misconfiguration, not a criterion-2 failure.

---

## Criterion 3 — `Ctrl+F8` writes `clip-*.mp4` within 2 s of post-roll completion

**Command**

With the buffer running, start a stopwatch, press **Ctrl+F8** at a known instant, and
watch the log.

**Expected observation (PASS)**

Immediately after the keypress:

```
hotkey pressed at <trigger_ms>ms; waiting for post-roll
```

Then, once the post-roll has elapsed (about `post_seconds` later):

```
wrote <path> (<duration_ms>ms, <size_bytes> bytes, encoder=<name>)
```

- `<path>` ends with `clip-<unix_seconds>.mp4` and the file exists in
  `%LOCALAPPDATA%\localplay\clips\`.
- The **wall-clock delta from your keypress to the `wrote` line** is **less than
  `post_seconds + 2 s`**. The trigger→write path includes waiting out the rest of the
  post-roll, up to 50 ms of polling granularity, and the splice; the 2 s budget is on top
  of `post_seconds`.

To measure precisely, subtract the timestamp the log formatter prints at the start of the
`wrote` line from your recorded keypress time (or just use a stopwatch).

**FAILURE looks like**

- The `wrote` line never appears after the press — e.g.
  `Error: timed out waiting for post-roll (span=<ms> need=<ms>)` (the post-roll never
  completed).
- `wrote` appears but the delta is **≥ `post_seconds + 2 s`**.
- No new `clip-*.mp4` appears on disk, or the file is not named `clip-*.mp4`.

**Do not misread:** the budget is `post_seconds + 2 s`, not `2 s`. With `post_seconds = 5`
a delta of 6.8 s is a **PASS**. Count the delta from the keypress to the `wrote` line
only.

---

## Criterion 4 — Clip duration matches `pre_seconds + post_seconds` (±0.5 s)

**Command**

```powershell
ffprobe -v error -show_format -of json "%LOCALAPPDATA%\localplay\clips\clip-<ts>.mp4"
```

Read `format.duration` (seconds).

**Expected observation (PASS)**

`format.duration` ≈ `pre_seconds + post_seconds`, within **±0.5 s**. With the defaults
that is ≈ 35.0 s.

Clips are whole segments concatenated with a stream copy (no re-encode, no leading trim),
and cuts snap to segment/keyframe boundaries (`segment_time`). So the duration is
quantised to `segment_time`; the ±0.5 s tolerance covers that step plus container
rounding. With `segment_time = 1` the achievable steps are 1 s. If you raise
`segment_time`, expect coarser quantisation — either lower it back to 1 s for this check,
or widen your expectation accordingly.

**FAILURE looks like**

- `format.duration` ≈ `pre_seconds` only, or ≈ `post_seconds` only — one side of the
  window is missing.
- `format.duration` ≈ 0, or ffprobe errors, or there is no `format.duration`.
- With `segment_time = 1`, the duration is off by more than 0.5 s (e.g. 33 s or 37 s when
  expecting 35).

**Do not misread:** `format.duration` is a decimal string of seconds; `34.8` is within
tolerance of `35.0`. Do not confuse `format.duration` with a per-`stream` `duration` field.

---

## Criterion 5 — The clip is a stream copy, not a re-encode

**Command**

```powershell
ffprobe -v error -show_streams -show_format -of json "%LOCALAPPDATA%\localplay\clips\clip-<ts>.mp4"
```

Compare against the encoder the CLI logged: the `encoder=<name>` field on the `wrote …`
line, and the startup `encoding with <name>` line.

**Expected observation (PASS)**

- The clip's **video** stream `codec_name` matches the codec *family* of the logged
  encoder:

  | Logged encoder | Expected `codec_name` |
  |---|---|
  | `h264_nvenc`, `h264_qsv`, `h264_amf` | `h264` |
  | `hevc_nvenc`, `hevc_qsv`, `hevc_amf` | `hevc` |

  ffprobe reports the codec, not the encoder implementation, so it never prints the
  `_nvenc`/`_qsv`/`_amf` suffix — matching on the family is correct.
- **Extraction completed near-instantly.** The gap between the post-roll finishing
  (keypress time + `post_seconds`) and the `wrote` line is a small fraction of a second —
  materially shorter than the clip's own duration. A re-encode of a 35 s clip would take
  on the order of tens of seconds (CPU re-encoding runs far slower than real time), which
  is the whole point of stream copy.

Why this proves it: the clip is built by concatenating whole segments with `-c copy` — no
decode and no encode runs on it. The two independent pieces of evidence are (a) the clip's
codec family equals the hardware encoder that produced the segments, and (b) the
extraction is far faster than the clip's duration. A re-encode would show the time scaling
with duration instead.

**FAILURE looks like**

- Codec mismatch — e.g. the clip is `hevc` while the log says `encoder=h264_nvenc`; or the
  clip's codec is `h264` but the log shows a software encoder name (which would itself mean
  the "no CPU fallback" rule was violated).
- Extraction takes a significant fraction of the clip's duration (e.g. more than a few
  seconds for a 35 s clip) — something decoded and re-encoded.
- The `wrote … encoder=` value does not match the startup `encoding with …` value.

**Do not misread:** ffprobe will never literally print `h264_nvenc`; `codec_name: h264` is
the expected, correct match. The `_nvenc`/`_qsv`/`_amf` suffix appears only in the CLI log.

---

## Criterion 6 — Steady-state CPU under 5% of one core and RSS under 400 MB

**Command** (PowerShell, while the buffer is running and has settled)

```powershell
$x1 = Get-Process localplay-cli
$cpu1 = $x1.TotalProcessorTime.TotalSeconds
$t1 = Get-Date
Start-Sleep -Seconds 60
$x2 = Get-Process localplay-cli
$cpu2 = $x2.TotalProcessorTime.TotalSeconds

"cores used : {0}" -f [math]::Round(($cpu2-$cpu1)/((Get-Date)-$t1).TotalSeconds, 3)
"RSS (MB)   : {0}" -f [math]::Round($x2.WorkingSet64/1MB, 1)
```

**Expected observation (PASS)**

- `cores used` **< 0.05** — i.e. under 5% of one core.
- `RSS (MB)` **< 400**.

Measured at steady state: after the ring is full and startup has passed (sample after the
first 30–60 s), not during startup.

**How the numbers map to the thresholds**

- "5% of one core" = `0.05` of a single logical core. `ΔCPU/Δt` is cores used, so
  `< 0.05` passes.
- RSS = `WorkingSet64` (bytes); the threshold is 400 MB = `419430400` bytes.
- Task Manager's **CPU** column is normalised across all cores — multiply its reading by
  your logical-processor count to get "% of one core". `Get-Counter`'s
  `\Process(localplay-cli)\% Processor Time` reports `100` = one core, so the threshold
  there is `< 5`. Task Manager's **Memory (active private working set)** column is the
  figure to compare against 400 MB.

**FAILURE looks like**

- `cores used` sustained at or above `0.05`, or `RSS (MB)` sustained at or above 400.
- CPU spiking continuously (not just at startup) — suggests CPU re-encoding or a
  busy-wait.

**Do not misread:** a **debug** build's CPU/RSS are not representative and may fail while
release passes — always sample the `--release` binary. Startup spikes (ffmpeg spawn,
encoder init, first probes) are not the steady-state figure; sample later.

---

## Criterion 7 — Non-zero exit with an actionable message when no hardware encoder exists

**Command**

Force `vendor` to an encoder this machine does **not** have, then run. On an AMD-only box
(pick a value step 0.2 reported `absent`):

```powershell
# in %LOCALAPPDATA%\localplay\config.toml set:
#   vendor = "nvenc"
cargo run --release -p localplay-cli -- buffer
```

**Expected observation (PASS)**

- The process starts, cannot find the requested encoder, prints an error, and exits
  **non-zero**. Confirm the exit code: `$LASTEXITCODE` (PowerShell) or `%ERRORLEVEL%`
  (cmd) is not `0`.
- The error **names the missing encoder**, e.g.:

  ```
  Error: no usable hardware encoder. ffmpeg advertises none of: h264_nvenc. Install the
  GPU vendor runtime (NVIDIA driver / Intel graphics driver / AMD Adrenalin), or set
  encode.vendor to a vendor this machine has. localplay will not fall back to CPU
  encoding because it would cost game performance.
  ```

- Nothing starts: no `WGC capture started …` line, no `seg-*.mp4`, no `buffering …` line,
  no clip.

**FAILURE (each of these is a defect, not a pass)**

- The process **exits 0**, keeps running, or produces a clip. That is a **silent CPU
  fallback**, which violates the project's non-negotiable principle against CPU encoding
  — a defect.
- It exits non-zero but the message does not name the missing encoder (e.g. a generic
  "ffmpeg failed").
- It silently proceeds with a *different* hardware vendor than the one requested, without
  saying so.

**Do not misread:** the message names the **ffmpeg encoder id** the codec resolves to
(`h264_nvenc` for `codec = "h264"`, or `hevc_nvenc` for HEVC), not the vendor word
"NVIDIA". Look for `h264_nvenc` / `hevc_nvenc` (and the `*_qsv` / `*_amf` equivalents).

---

## Criterion 8 — The clip contains a synchronised audio stream

**Command**

```powershell
ffprobe -v error -show_streams -show_format -of json "%LOCALAPPDATA%\localplay\clips\clip-<ts>.mp4"
```

Then play the clip — e.g. `ffplay "%LOCALAPPDATA%\localplay\clips\clip-<ts>.mp4"`, or any
player — and listen.

**Expected observation (PASS)**

- `ffprobe` lists **exactly one** stream with `codec_type: "video"` and **exactly one**
  with `codec_type: "audio"`.
- Video `codec_name` is `h264` (or `hevc`, per config); audio `codec_name` is `aac`.
- On playback the audio is **audible** and **lip-sync is correct** — play a sound that is
  tied to a visible on-screen event and check the sound lines up with the picture.

**Drift:** the design says A/V drift is logged per clip, and it is. Every splice emits an
`info`-level line, so it is visible at the default `RUST_LOG=info`:

```
clip clip-<ts>: video <v>ms audio <a>ms drift <delta>ms
```

`<v>` and `<a>` are the two streams' durations in the produced clip. `<delta>` is
`(video start + video duration) − (audio start + audio duration)` — how much later the
video timeline ends than the audio's. **A negative `<delta>` means the audio outlasts the
video.**

What is healthy: on the real 60 fps pipeline both streams are cut on the same segment
boundaries, so the offset is small — **tens of milliseconds, comfortably under ~100 ms** —
and it does **not grow** with clip length. What is unhealthy: a **multi-hundred-ms or
second-scale** offset, above all one that **grows** as `pre_seconds + post_seconds`
increases — that is the two live sources genuinely desyncing, not muxing quantisation.

**Honest limitation:** this drift is measured **within the produced clip**. It does **not**
measure the QPC-clock divergence between the two *live* capture sources (the WGC video
clock vs the WASAPI audio clock); that would require instrumenting the capture path
itself, which this build does not do. The line proves the muxed clip's two streams line up;
it is not a check of the live source clocks. (See [Known gaps](#known-gaps).)

**FAILURE looks like**

- Zero audio streams, more than one audio stream, or more than one video stream.
- An audio stream is present but **silent** — WASAPI loopback captured nothing.
- Lip-sync is visibly/tangibly off (audio leads or trails the video).
- The drift line reports a **multi-hundred-ms or second-scale** `<delta>`, or `<delta>`
  grows with clip length across successive clips — a real desync.
- `ffprobe` errors on the clip.

**Do not misread:** a normal clip has exactly two streams (one video, one audio). The
`clip-<ts>.concat.txt` intermediate and the scratch `seg-*.mp4` files are inputs to the
splice, not what you probe — probe the finished `clip-*.mp4`.

---

## Known gaps

These are unverified or broken areas of the current build. They are stated plainly and
not softened; do not read a pass elsewhere as coverage of them.

- **The Windows backends have never been executed.** `crates/capture/src/wgc.rs` and
  `crates/capture/src/wasapi.rs` have only ever been **type-checked**, via
  `cargo check --target x86_64-pc-windows-msvc` from macOS. Neither has ever run on
  Windows; **no frame has ever been captured**, and no audio sample has ever come through
  WASAPI. This runbook is the first place they get a real run.
- **The audio mix-format gate fails loudly on a non-48 kHz endpoint.** WASAPI loopback
  only produces 48 kHz stereo, and there is **no resampling**. A default playback device
  whose shared-mode mix format is not 48 kHz is refused **by name** at startup ("… is
  <rate>Hz, but localplay captures at 48000Hz and does not resample yet …"). Set the
  device to 48000 Hz in Windows Sound settings.
- **Live-source clock divergence is still unmeasured.** A/V drift *within each produced
  clip* is now logged (see criterion 8), but that only checks that the two muxed streams
  line up. The divergence between the two *live* capture clocks — the WGC video clock and
  the WASAPI audio clock, both nominally QPC-based — is **not** instrumented anywhere, so
  a slow live-source desync that the segment muxing happens to reshape would not be
  caught here.
- **The capture frame pool is never recreated on a display mode change.**
  `Direct3D11CaptureFramePool::Recreate` is not called; a mid-capture resolution or
  refresh-rate change is not handled. The backend copies whatever size the incoming
  texture happens to be, but the pool itself is never resized.
- **Encoder selection only checks ffmpeg's advertisement.** `cargo xtask probe` (and the
  CLI's vendor selection) tests that `ffmpeg -encoders` lists the encoder; the 1-frame
  smoke test that would prove the encoder actually opens on your driver is deferred to
  Phase 2. `listed` is necessary but not sufficient.
- **Non-Windows runs prove none of this.** Off Windows the pipeline uses `StubCapture`,
  which captures a synthetic image and nothing real. A green macOS/Linux test run is not
  evidence for any of the eight criteria.

---

## How to record the result

Write the measurements to `docs/runbooks/phase-1-results-<date>.md` (or a comment on the
tracking issue) — one line per criterion, with the measured value and PASS/FAIL. Record,
specifically:

- **Environment:** ffmpeg version (`ffmpeg -version`) and the GPU + driver version, because
  encoder behaviour is driver-dependent.
- **Criterion 1:** the WGC display name and the captured `W×H`; the configured `fps`; and
  whether `frames=` grew at roughly `fps × seconds` and `span=` tracked real time over the
  run (yes/no).
- **Criterion 2:** the configured `scratch_cap_bytes`; the largest `bytes=` seen; and the
  largest the scratch directory measured (note if it was exactly one segment over).
- **Criterion 3:** `post_seconds`; the measured keypress→`wrote` delta; pass/fail against
  `post_seconds + 2 s`.
- **Criterion 4:** the expected `pre_seconds + post_seconds`; the `format.duration` seen;
  the absolute difference.
- **Criterion 5:** the logged encoder name; the clip's video `codec_name`; the approximate
  extraction wall time and the clip duration.
- **Criterion 6:** `cores used`; RSS in MB; and the machine's logical-processor count (to
  interpret Task Manager).
- **Criterion 7:** the forced `vendor`; the process exit code; and the exact error text.
- **Criterion 8:** the stream counts (video/audio); the codec names; whether lip-sync was
  correct on playback; and the `<delta>` from each clip's `clip … drift` log line (note
  whether it stayed sub-100 ms or grew).

Do **not** mark a criterion PASS unless you personally observed it on Windows. Where a run
was skipped or blocked, write that down.
