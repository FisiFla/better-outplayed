# localplay — Phase 1 (Replay Buffer PoC) Implementation Plan

> **For agentic workers:** implement this plan task-by-task — dispatch a fresh subagent per task with the native `task` tool (recommended for quality), or use the superpowers-executing-plans skill to work through it inline. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a headless Windows binary that keeps a bounded replay buffer of the
last N seconds of screen video and system audio on disk, and writes a lossless clip to
a file the instant `Ctrl+F8` is pressed.

**Architecture:** WGC captures frames and WASAPI loopback captures PCM; both feed a
single `ffmpeg` child process over two pipes. ffmpeg encodes to hardware H.264 and
writes 1-second MP4 segments into a scratch directory. Rust owns a ledger of completed
segments and evicts oldest-first against a byte cap. A trigger resolves a segment
window and concatenates whole segments with `-c copy` — no re-encode, ever.

**Tech Stack:** Rust 2021 · Cargo workspace · `windows-rs` (WGC/WASAPI/hotkeys) ·
`ffmpeg`/`ffprobe` sidecars · `rusqlite` (bundled) · `serde`/`toml` · `tracing`

**Spec:** [`../specs/2026-09-23-localplay-design.md`](../specs/2026-09-23-localplay-design.md)

---

## Spec refinements adopted during planning

Three deliberate deviations from the spec, all simplifications. Each is recorded here
because the spec is the approved contract.

1. **No leading-segment trim.** The spec (§6.2 step 4) says to trim the first segment
   to the exact pre-roll offset with `-ss`/`-c copy`. This is a **no-op**: `-ss` with
   `-c copy` seeks to the nearest *preceding keyframe*, and every segment boundary is
   already a keyframe. Trimming the first segment therefore produces byte-identical
   output to simply starting at that segment. We drop the trim and clamp the window to
   segment boundaries. Fewer moving parts, one pass, no temp files.
2. **Segment boundary is the only cut granularity.** Pre-roll accuracy is therefore
   `± segment_time` (1 s by default), which the spec already accepts (§6.3).
3. **Hardware encoder selection never returns a software encoder.** The `encoder`
   crate can *express* a software encoder behind a `test-encoders` feature so the
   pipeline is testable on the macOS dev host, but the config parser rejects
   `vendor = "software"` and the probe never selects it. This keeps the spec's "no
   silent CPU fallback" guarantee while making the pipeline testable off-Windows.

---

## Prerequisites

- **Windows target machine** for Tasks 14, 15 and 17 (and to execute the acceptance
  criteria). Tasks 1–13, 16 run on any platform with `ffmpeg` available.
- **macOS dev host:** `brew install ffmpeg` — needed for Tasks 2–4, 9–10, 16. This
  installs a system ffmpeg used *only* for development and tests. Shipped builds use
  the sidecar from Task 16.
- Rust stable with the MSVC toolchain on Windows.

---

## File Structure

| File | Responsibility |
|---|---|
| `Cargo.toml` | Workspace members, shared dependency versions, shared lints |
| `crates/media/src/binaries.rs` | Locate `ffmpeg`/`ffprobe`; run them with timeout + captured stderr |
| `crates/media/src/probe.rs` | `MediaInfo` from `ffprobe -print_format json` |
| `crates/media/src/edit.rs` | `remux_lossless`, `trim_lossless`, `thumbnail` |
| `crates/replay/src/ledger.rs` | `Segment`, `SegmentLedger` — the ring index and eviction math |
| `crates/replay/src/scanner.rs` | Decide which segments on disk are complete |
| `crates/replay/src/window.rs` | Resolve a trigger instant into a `SegmentWindow` |
| `crates/replay/src/splice.rs` | `ClipSplicer` — concat + lossless remux |
| `crates/replay/src/buffer.rs` | `RingBuffer` — owns the ffmpeg child, eviction, trigger orchestration |
| `crates/capture/src/lib.rs` | `CaptureBackend` / `AudioBackend` traits, `Frame`, `AudioBuffer` |
| `crates/capture/src/stub.rs` | `StubCapture`, `StubAudio` — non-Windows, deterministic |
| `crates/capture/src/wgc.rs` | WGC video backend (Windows only) |
| `crates/capture/src/wasapi.rs` | WASAPI loopback audio backend (Windows only) |
| `crates/encoder/src/lib.rs` | `Encoder` trait, `EncodeConfig`, `Vendor`, `VideoCodec` |
| `crates/encoder/src/ffmpeg.rs` | `FfmpegEncoder` — spawns one ffmpeg child, two pipe inputs |
| `crates/encoder/src/probe.rs` | `available_vendors()` via `ffmpeg -encoders` + smoke test |
| `crates/store/src/lib.rs` | Connection, migrations, `insert_clip`, `list_clips` |
| `crates/events/src/lib.rs` | `Trigger`, `EventKind` |
| `crates/events/src/hotkey.rs` | `RegisterHotKey` listener (Windows only) |
| `apps/localplay-cli/src/config.rs` | Root `Config`, TOML load, validation |
| `apps/localplay-cli/src/main.rs` | `buffer` subcommand: wiring, stats, soak loop |
| `xtask/src/main.rs` | `sidecars` fetch/verify, `probe` encoder listing |

---

## Task 1: Workspace skeleton

**Files:**
- Create: `Cargo.toml`, `crates/{capture,encoder,replay,media,events,store}/Cargo.toml`, `apps/localplay-cli/Cargo.toml`, `xtask/Cargo.toml`, and a `src/lib.rs` or `src/main.rs` for each

- [ ] **Step 1: Create the workspace manifest**

```toml
# Cargo.toml
[workspace]
resolver = "2"
members = [
    "crates/capture",
    "crates/encoder",
    "crates/replay",
    "crates/media",
    "crates/events",
    "crates/store",
    "apps/localplay-cli",
    "xtask",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "MIT OR Apache-2.0"
repository = "https://github.com/FisiFla/localplay"

[workspace.dependencies]
anyhow = "1"
thiserror = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tempfile = "3"

[workspace.lints.rust]
unsafe_op_in_unsafe_fn = "deny"

[workspace.lints.clippy]
# `warn`, not `deny`: test bodies legitimately use `unwrap()` everywhere, and
# denying it would make `cargo test` fail to compile.
unwrap_used = "warn"
```

- [ ] **Step 2: Create each crate**

Each library crate gets this shape (substituting name/path):

```toml
# crates/media/Cargo.toml
[package]
name = "localplay-media"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
anyhow.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true

[dev-dependencies]
tempfile.workspace = true

[lints]
workspace = true
```

`crates/capture/Cargo.toml` additionally carries the Windows dependency:

```toml
[target.'cfg(windows)'.dependencies]
windows = { version = "0.58", features = [] }

[features]
test-encoders = []
```

> Add the real `windows` feature list in Task 14 with `cargo add windows --features ...`;
> do not hand-guess the version.

`crates/replay/Cargo.toml` needs media for splicing and both test-only sources:

```toml
[package]
name = "localplay-replay"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
anyhow.workspace = true
localplay-media = { path = "../media" }
serde.workspace = true
thiserror.workspace = true
toml.workspace = true
tracing.workspace = true

[dev-dependencies]
localplay-capture = { path = "../capture" }
localplay-encoder = { path = "../encoder" }
tempfile.workspace = true

# Forwards to the encoder's feature so the pipeline is testable off-Windows.
[features]
test-encoders = ["localplay-encoder/test-encoders"]

[lints]
workspace = true
```

`crates/encoder/Cargo.toml`:

```toml
[package]
name = "localplay-encoder"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
anyhow.workspace = true
localplay-capture = { path = "../capture" }
localplay-media = { path = "../media" }
tracing.workspace = true

[dev-dependencies]
tempfile.workspace = true

[features]
test-encoders = []

[lints]
workspace = true
```

`crates/events/Cargo.toml`:

```toml
[package]
name = "localplay-events"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
anyhow.workspace = true
tracing.workspace = true

[target.'cfg(windows)'.dependencies]
windows = { version = "0.58", features = [] }

[lints]
workspace = true
```

`crates/store/Cargo.toml`:

```toml
[package]
name = "localplay-store"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
anyhow.workspace = true
rusqlite = { version = "0.32", features = ["bundled"] }

[lints]
workspace = true
```

Each `src/lib.rs` starts minimal:

```rust
//! localplay media drivers.
```

`xtask/Cargo.toml` (the plan's file list names it but no contents were given, so this
is the canonical shape):

```toml
[package]
name = "xtask"
version.workspace = true
edition.workspace = true
license.workspace = true

[lints]
workspace = true
```

`xtask/src/main.rs` is a doc comment plus an empty `fn main() {}` for now; Task 16
fills it in.

`apps/localplay-cli/Cargo.toml`:

```toml
[package]
name = "localplay-cli"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
anyhow.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
serde.workspace = true
toml.workspace = true
localplay-capture = { path = "../../crates/capture" }
localplay-encoder = { path = "../../crates/encoder" }
localplay-replay  = { path = "../../crates/replay" }
localplay-media   = { path = "../../crates/media" }
localplay-events  = { path = "../../crates/events" }
localplay-store   = { path = "../../crates/store" }

# Enables the hidden `--dev-software-encoder` flag for pipeline smoke tests on a
# host with no GPU encoder. Never enabled in a release build.
[features]
test-encoders = ["localplay-encoder/test-encoders"]

[lints]
workspace = true
```

- [ ] **Step 3: Add the two missing crate names to the workspace dependency table**

Append to the `[workspace.dependencies]` block in the root `Cargo.toml`:

```toml
localplay-capture = { path = "crates/capture" }
localplay-encoder = { path = "crates/encoder" }
localplay-replay  = { path = "crates/replay" }
localplay-media   = { path = "crates/media" }
localplay-events  = { path = "crates/events" }
localplay-store   = { path = "crates/store" }
```

- [ ] **Step 4: Verify the workspace builds**

Run: `cargo check --workspace`
Expected: `Finished` with no errors. Warnings about unused crates are fine.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "build: scaffold the cargo workspace and crate skeletons"
```

---

## Task 2: Locate the ffmpeg binaries

**Files:**
- Create: `crates/media/src/binaries.rs`
- Modify: `crates/media/src/lib.rs`
- Test: `crates/media/src/binaries.rs` (inline `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn explicit_path_wins() {
        let found = FfmpegBinaries::discover_from(Some(PathBuf::from("/custom/dir")), &[])
            .expect("explicit path is used verbatim");
        assert_eq!(found.ffmpeg, PathBuf::from("/custom/dir/ffmpeg"));
    }

    #[test]
    fn falls_back_to_path_lookup_and_reports_all_searched_locations() {
        let err = FfmpegBinaries::discover_from(None, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("sidecar"), "error must name the sidecar dir: {msg}");
        assert!(msg.contains("PATH"), "error must name PATH: {msg}");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p localplay-media binaries`
Expected: FAIL — `cannot find struct FfmpegBinaries` / `cannot find function discover_from`.

- [ ] **Step 3: Implement**

```rust
//! Locating and invoking the ffmpeg sidecar binaries.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

/// The directory ffmpeg/ffprobe are expected to live in for a packaged build.
pub fn sidecar_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("binaries")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegBinaries {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl FfmpegBinaries {
    /// Resolve binaries: explicit dir, then sidecar dir, then `PATH`.
    pub fn discover(explicit: Option<PathBuf>) -> Result<Self> {
        let searched = vec![
            explicit.clone(),
            Some(sidecar_dir()),
            which_dir("ffmpeg"),
        ];
        let searched: Vec<PathBuf> = searched.into_iter().flatten().collect();
        Self::discover_from(explicit, &searched)
    }

    /// Testable core: `candidates` is the list of directories that were searched.
    /// Panics are avoided so the error message can list every location tried.
    pub fn discover_from(explicit: Option<PathBuf>, candidates: &[PathBuf]) -> Result<Self> {
        // An explicit directory is authoritative: if the caller named it, use it.
        if let Some(dir) = explicit {
            return Ok(Self {
                ffmpeg: dir.join(exe("ffmpeg")),
                ffprobe: dir.join(exe("ffprobe")),
            });
        }
        if candidates.is_empty() {
            bail!(
                "ffmpeg not found. Looked in the sidecar directory ({}), PATH, and any \
                 configured location. Run `cargo xtask sidecars fetch` or install ffmpeg.",
                sidecar_dir().display()
            );
        }
        for dir in candidates {
            let ffmpeg = dir.join(exe("ffmpeg"));
            let ffprobe = dir.join(exe("ffprobe"));
            if ffmpeg.is_file() && ffprobe.is_file() {
                return Ok(Self { ffmpeg, ffprobe });
            }
        }
        bail!(
            "ffmpeg not found. Searched: {}. Run `cargo xtask sidecars fetch` or install ffmpeg.",
            candidates
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn exe(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// Directory containing `ffmpeg` on `PATH`, if any.
fn which_dir(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .find(|dir| dir.join(exe(binary)).is_file())
}

/// Run a child with stdin closed and a hard timeout, capturing stdout/stderr.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawning ffmpeg child")?;

    // Poll rather than block, so a hung ffmpeg cannot wedge the buffer.
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait().context("polling ffmpeg child")? {
            Some(_) => break,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("ffmpeg timed out after {timeout:?}");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    child.wait_with_output().context("collecting ffmpeg output")
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p localplay-media binaries`
Expected: `test result: ok. 2 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/media
git commit -m "feat(media): locate ffmpeg/ffprobe sidecars with actionable errors"
```

---

## Task 3: `ffprobe` into `MediaInfo`

**Files:**
- Create: `crates/media/src/probe.rs`
- Modify: `crates/media/src/lib.rs`
- Test: `crates/media/src/probe.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_combined_audio_video_probe() {
        let json = r#"{
          "streams": [
            {"codec_type":"video","codec_name":"h264","width":320,"height":240,"bit_rate":"500000"},
            {"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2}
          ],
          "format": {"duration":"3.033333","size":"190000"}
        }"#;
        let info = MediaInfo::from_ffprobe_json(json).expect("valid probe json");

        assert_eq!(info.video.as_ref().map(|v| v.codec.as_str()), Some("h264"));
        assert_eq!(info.video.as_ref().map(|v| (v.width, v.height)), Some((320, 240)));
        assert_eq!(info.audio.as_ref().map(|a| a.codec.as_str()), Some("aac"));
        assert_eq!(info.audio.as_ref().map(|a| a.channels), Some(2));
        assert_eq!(info.duration_ms, 3033);
        assert_eq!(info.size_bytes, 190000);
    }

    #[test]
    fn rejects_a_probe_with_no_streams() {
        let json = r#"{"streams":[],"format":{"duration":"0.0","size":"0"}}"#;
        assert!(MediaInfo::from_ffprobe_json(json).is_err());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p localplay-media probe`
Expected: FAIL — `cannot find type MediaInfo`.

- [ ] **Step 3: Implement**

```rust
//! `ffprobe` JSON into a typed `MediaInfo`.

use crate::binaries::{run_with_timeout, FfmpegBinaries};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoStream {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub bit_rate: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioStream {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaInfo {
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub video: Option<VideoStream>,
    pub audio: Option<AudioStream>,
}

#[derive(Deserialize)]
struct RawProbe {
    streams: Vec<RawStream>,
    format: RawFormat,
}

#[derive(Deserialize)]
struct RawStream {
    codec_type: String,
    codec_name: String,
    width: Option<u32>,
    height: Option<u32>,
    bit_rate: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u16>,
}

#[derive(Deserialize)]
struct RawFormat {
    duration: Option<String>,
    size: Option<String>,
}

impl MediaInfo {
    pub fn from_ffprobe_json(json: &str) -> Result<Self> {
        let raw: RawProbe = serde_json::from_str(json).context("parsing ffprobe json")?;
        if raw.streams.is_empty() {
            bail!("ffprobe reported no streams");
        }

        let video = raw
            .streams
            .iter()
            .find(|s| s.codec_type == "video")
            .map(|s| VideoStream {
                codec: s.codec_name.clone(),
                width: s.width.unwrap_or(0),
                height: s.height.unwrap_or(0),
                bit_rate: s.bit_rate.as_deref().and_then(|v| v.parse().ok()),
            });

        let audio = raw
            .streams
            .iter()
            .find(|s| s.codec_type == "audio")
            .map(|s| AudioStream {
                codec: s.codec_name.clone(),
                sample_rate: s.sample_rate.as_deref().and_then(|v| v.parse().ok()).unwrap_or(0),
                channels: s.channels.unwrap_or(0),
            });

        Ok(Self {
            duration_ms: seconds_to_ms(raw.format.duration.as_deref()),
            size_bytes: raw.format.size.as_deref().and_then(|v| v.parse().ok()).unwrap_or(0),
            video,
            audio,
        })
    }

    /// Run `ffprobe` against a file on disk.
    pub fn probe(bin: &FfmpegBinaries, path: &Path) -> Result<Self> {
        let mut cmd = Command::new(&bin.ffprobe);
        cmd.args([
            "-v", "error",
            "-print_format", "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path);
        let out = run_with_timeout(cmd, PROBE_TIMEOUT)?;
        if !out.status.success() {
            bail!(
                "ffprobe failed on {}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Self::from_ffprobe_json(&String::from_utf8_lossy(&out.stdout))
    }
}

/// ffprobe reports duration as a decimal string of seconds.
fn seconds_to_ms(seconds: Option<&str>) -> u64 {
    seconds
        .and_then(|s| s.parse::<f64>().ok())
        .map(|s| (s * 1000.0).round() as u64)
        .unwrap_or(0)
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p localplay-media probe`
Expected: `test result: ok. 2 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/media
git commit -m "feat(media): parse ffprobe json into MediaInfo"
```

---

## Task 4: Lossless remux and trim

**Files:**
- Create: `crates/media/src/edit.rs`
- Modify: `crates/media/src/lib.rs`
- Test: `crates/media/tests/lossless.rs`

- [ ] **Step 1: Write the failing test**

```rust
//! Proves the spec's principle 5: export never re-encodes.
use localplay_media::{edit, probe::MediaInfo, FfmpegBinaries};
use std::process::Command;

fn ffmpeg() -> FfmpegBinaries {
    FfmpegBinaries::discover(None).expect("ffmpeg on PATH (brew install ffmpeg)")
}

/// 3s of testsrc2 video + 440Hz sine audio, so there is a real audio stream to preserve.
fn fixture(dir: &std::path::Path) -> std::path::PathBuf {
    let out = dir.join("fixture.mp4");
    let bin = ffmpeg();
    let status = Command::new(&bin.ffmpeg)
        .args([
            "-v", "error", "-y",
            "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30",
            "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
            "-t", "3",
            "-c:v", "libx264", "-preset", "ultrafast", "-g", "30",
            // `-ac 2` matters: `sine` defaults to mono, but the real pipeline
            // captures 48kHz stereo, so a mono fixture would not exercise the
            // same muxing path (criterion 8).
            "-c:a", "aac", "-ac", "2", "-shortest",
        ])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "fixture generation failed");
    out
}

#[test]
fn remux_preserves_codec_and_produces_playable_output() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let src = fixture(dir.path());
    let dst = dir.path().join("remuxed.mp4");

    edit::remux_lossless(&bin, &src, &dst).expect("remux");

    let before = MediaInfo::probe(&bin, &src).unwrap();
    let after = MediaInfo::probe(&bin, &dst).unwrap();
    assert_eq!(before.video.as_ref().unwrap().codec, after.video.as_ref().unwrap().codec);
    assert_eq!(before.audio.as_ref().unwrap().codec, after.audio.as_ref().unwrap().codec);
    let drift = before.duration_ms.abs_diff(after.duration_ms);
    assert!(drift < 100, "duration drifted {drift}ms");
}

#[test]
fn trim_keeps_both_streams_and_shortens() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let src = fixture(dir.path());
    let dst = dir.path().join("trimmed.mp4");

    edit::trim_lossless(&bin, &src, &dst, 1000, 2000).expect("trim");

    let after = MediaInfo::probe(&bin, &dst).unwrap();
    assert!(after.video.is_some(), "video stream must survive a trim");
    assert!(after.audio.is_some(), "audio stream must survive a trim");
    // Keyframe snapping means this is approximate, not exact.
    assert!(
        (900..=2100).contains(&after.duration_ms),
        "trimmed duration {}ms outside tolerance",
        after.duration_ms
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p localplay-media --test lossless`
Expected: FAIL — `cannot find function remux_lossless`.

- [ ] **Step 3: Implement**

```rust
//! Lossless (stream-copy) media edits. These never re-encode.

use crate::binaries::{run_with_timeout, FfmpegBinaries};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const EDIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Remux without re-encoding, moving the index to the front for fast seeking.
pub fn remux_lossless(bin: &FfmpegBinaries, src: &Path, dst: &Path) -> Result<()> {
    let mut cmd = Command::new(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-i"])
        .arg(src)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(dst);
    expect_success(cmd, "remux", dst)
}

/// Trim `[start_ms, end_ms)` with `-c copy`.
///
/// Cuts snap to the nearest preceding keyframe — this is inherent to stream copy,
/// not a bug. See spec §6.3.
pub fn trim_lossless(
    bin: &FfmpegBinaries,
    src: &Path,
    dst: &Path,
    start_ms: u64,
    end_ms: u64,
) -> Result<()> {
    if end_ms <= start_ms {
        bail!("trim range is empty: start={start_ms}ms end={end_ms}ms");
    }
    let mut cmd = Command::new(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-ss"])
        .arg(format!("{:.3}", start_ms as f64 / 1000.0))
        .arg("-i")
        .arg(src)
        .arg("-t")
        .arg(format!("{:.3}", (end_ms - start_ms) as f64 / 1000.0))
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(dst);
    expect_success(cmd, "trim", dst)
}

/// Single-frame JPEG at `at_ms`, for Phase 2 timelines.
pub fn thumbnail(bin: &FfmpegBinaries, src: &Path, at_ms: u64, dst: &Path) -> Result<()> {
    let mut cmd = Command::new(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-ss"])
        .arg(format!("{:.3}", at_ms as f64 / 1000.0))
        .arg("-i")
        .arg(src)
        .args(["-frames:v", "1", "-q:v", "4"])
        .arg(dst);
    expect_success(cmd, "thumbnail", dst)
}

/// Concatenate segments into one file with `-c copy`.
///
/// Every segment must already share identical codec parameters — they do, because
/// they come from one encoder invocation (spec §6.1).
pub fn concat_lossless(bin: &FfmpegBinaries, list_file: &Path, dst: &Path) -> Result<()> {
    let mut cmd = Command::new(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(list_file)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(dst);
    expect_success(cmd, "concat", dst)
}

fn expect_success(mut cmd: Command, what: &str, dst: &Path) -> Result<()> {
    let out = run_with_timeout(cmd, EDIT_TIMEOUT)?;
    if !out.status.success() {
        bail!(
            "{what} failed writing {}: {}",
            dst.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if !dst.is_file() {
        bail!("{what} reported success but {} does not exist", dst.display());
    }
    dst.metadata()
        .map(|_| ())
        .with_context(|| format!("statting {}", dst.display()))
}
```

Add to `crates/media/src/lib.rs`:

```rust
//! localplay media drivers: locating ffmpeg and performing lossless edits.

pub mod binaries;
pub mod edit;
pub mod probe;

pub use binaries::FfmpegBinaries;
pub use probe::MediaInfo;
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p localplay-media --test lossless`
Expected: `test result: ok. 2 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/media
git commit -m "feat(media): lossless remux, trim, concat and thumbnails"
```

---

## Task 5: `SegmentLedger` and eviction

**Files:**
- Create: `crates/replay/src/ledger.rs`
- Modify: `crates/replay/src/lib.rs`
- Test: `crates/replay/src/ledger.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn seg(seq: u64, bytes: u64) -> Segment {
        Segment {
            seq,
            file: format!("seg-{seq:06}.mp4").into(),
            start_ms: seq * 1000,
            duration_ms: 1000,
            bytes,
        }
    }

    #[test]
    fn evicts_oldest_first_until_under_the_cap() {
        let mut l = SegmentLedger::default();
        for i in 0..5 {
            l.push(seg(i, 10));
        }
        assert_eq!(l.total_bytes(), 50);

        let evicted = l.evict_to_cap(25);

        assert_eq!(evicted.iter().map(|s| s.seq).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(l.total_bytes(), 20);
    }

    #[test]
    fn never_evicts_below_the_retained_minimum_even_when_over_cap() {
        let mut l = SegmentLedger::default();
        for i in 0..4 {
            l.push(seg(i, 100));
        }
        // Cap is unsatisfiable; a partial clip is worse than being over budget.
        let evicted = l.evict_to_cap(1);
        assert_eq!(l.len(), 1, "keeps the newest segment so a clip is still possible");
        assert_eq!(evicted.len(), 3);
    }

    #[test]
    fn keeps_segments_ordered_by_sequence() {
        let mut l = SegmentLedger::default();
        l.push(seg(2, 10));
        l.push(seg(0, 10));
        l.push(seg(1, 10));
        assert_eq!(l.seqs(), vec![0, 1, 2]);
    }

    #[test]
    fn round_trips_through_toml() {
        let mut l = SegmentLedger::default();
        l.push(seg(7, 42));
        let text = l.to_toml().unwrap();
        let back = SegmentLedger::from_toml(&text).unwrap();
        assert_eq!(back.seqs(), vec![7]);
        assert_eq!(back.total_bytes(), 42);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p localplay-replay ledger`
Expected: FAIL — `cannot find type SegmentLedger`.

- [ ] **Step 3: Implement**

```rust
//! The scratch ring index.
//!
//! This is the only thing that survives a crash. Media itself is plain appended
//! segment files — deliberately not memory-mapped (spec §6.4).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One completed segment on the scratch volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub seq: u64,
    pub file: PathBuf,
    /// Offset from capture start, in milliseconds.
    pub start_ms: u64,
    pub duration_ms: u64,
    pub bytes: u64,
}

impl Segment {
    pub fn end_ms(&self) -> u64 {
        self.start_ms + self.duration_ms
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SegmentLedger {
    segments: Vec<Segment>,
    total_bytes: u64,
}

impl SegmentLedger {
    pub fn push(&mut self, seg: Segment) {
        self.total_bytes += seg.bytes;
        self.segments.push(seg);
        // Segments are observed out of order in tests and on rescan.
        self.segments.sort_by_key(|s| s.seq);
        self.segments.dedup_by_key(|s| s.seq);
        self.recompute_bytes();
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn seqs(&self) -> Vec<u64> {
        self.segments.iter().map(|s| s.seq).collect()
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Newest segment's end offset, i.e. how much capture timeline is on disk.
    pub fn span_ms(&self) -> u64 {
        self.segments.last().map(Segment::end_ms).unwrap_or(0)
    }

    /// Drop oldest segments until `total_bytes <= cap`.
    ///
    /// Retains at least one segment: an empty ring can never satisfy a later trigger,
    /// so being slightly over budget is preferable to being unable to clip at all.
    pub fn evict_to_cap(&mut self, cap: u64) -> Vec<Segment> {
        let mut evicted = Vec::new();
        while self.total_bytes > cap && self.segments.len() > 1 {
            let oldest = self.segments.remove(0);
            self.total_bytes = self.total_bytes.saturating_sub(oldest.bytes);
            evicted.push(oldest);
        }
        evicted
    }

    fn recompute_bytes(&mut self) {
        self.total_bytes = self.segments.iter().map(|s| s.bytes).sum();
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string(self).context("serialising ledger")
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        let mut l: Self = toml::from_str(text).context("parsing ledger")?;
        l.recompute_bytes();
        l.segments.sort_by_key(|s| s.seq);
        Ok(l)
    }

    /// Persist atomically via temp file + rename, so a crash mid-write cannot
    /// leave a truncated ledger.
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        let text = self.to_toml()?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::from_toml(&text)
    }
}
```

Add to `crates/replay/src/lib.rs`:

```rust
//! localplay replay buffer: segment ledger, window resolution, clip splicing.

pub mod ledger;

pub use ledger::{Segment, SegmentLedger};
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p localplay-replay ledger`
Expected: `test result: ok. 4 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/replay
git commit -m "feat(replay): segment ledger with oldest-first eviction"
```

---

## Task 6: Complete-segment detection

**Files:**
- Create: `crates/replay/src/scanner.rs`
- Modify: `crates/replay/src/lib.rs`
- Test: `crates/replay/src/scanner.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_segment_is_complete_only_once_a_later_one_exists() {
        // seq 3 exists but ffmpeg may still be appending to it.
        assert_eq!(newly_complete(&[0, 1, 2, 3], None), vec![0, 1, 2]);
    }

    #[test]
    fn does_not_re_emit_already_known_segments() {
        // seq 3 is the max observed, so only 2 is newly complete and > the known 1.
        assert_eq!(newly_complete(&[0, 1, 2, 3], Some(1)), vec![2]);
    }

    #[test]
    fn nothing_is_complete_when_only_one_segment_exists() {
        assert_eq!(newly_complete(&[0], None), Vec::<u64>::new());
    }

    #[test]
    fn tolerates_out_of_order_and_duplicate_observations() {
        assert_eq!(newly_complete(&[2, 0, 2, 1], None), vec![0, 1]);
    }

    #[test]
    fn empty_directory_yields_nothing() {
        assert_eq!(newly_complete(&[], None), Vec::<u64>::new());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p localplay-replay scanner`
Expected: FAIL — `cannot find function newly_complete`.

- [ ] **Step 3: Implement**

```rust
//! Deciding which scratch segments are finished being written.

use std::collections::BTreeSet;

/// Sequence numbers observed on disk that are complete.
///
/// ffmpeg appends to the newest segment, so a segment is only trusted once a
/// strictly later one exists. Pure function: the caller does the filesystem walk.
pub fn newly_complete(observed: &[u64], highest_known: Option<u64>) -> Vec<u64> {
    let unique: BTreeSet<u64> = observed.iter().copied().collect();
    let Some(&max) = unique.iter().next_back() else {
        return Vec::new();
    };
    unique
        .into_iter()
        .filter(|seq| *seq < max)
        .filter(|seq| highest_known.is_none_or(|known| *seq > known))
        .collect()
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p localplay-replay scanner`
Expected: `test result: ok. 5 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/replay
git commit -m "feat(replay): detect complete scratch segments"
```

---

## Task 7: Trigger → segment window

**Files:**
- Create: `crates/replay/src/window.rs`
- Modify: `crates/replay/src/lib.rs`
- Test: `crates/replay/src/window.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Segment;

    fn ledger(n: u64) -> SegmentLedger {
        let mut l = SegmentLedger::default();
        for seq in 0..n {
            l.push(Segment {
                seq,
                file: format!("seg-{seq:06}.mp4").into(),
                start_ms: seq * 1000,
                duration_ms: 1000,
                bytes: 10,
            });
        }
        l
    }

    #[test]
    fn selects_whole_segments_covering_the_window() {
        // 5s pre-roll from t=10s over a 20s buffer => segments 5..=14.
        let w = resolve(&ledger(20), 10_000, 5_000, 5_000).unwrap();
        assert_eq!(w.seqs(), vec![5, 6, 7, 8, 9, 10, 11, 12, 13, 14]);
        assert!(!w.truncated_front);
    }

    #[test]
    fn clamps_and_reports_when_the_window_starts_before_the_buffer() {
        // 3s of buffer, 30s of pre-roll requested, 1s post-roll so the buffer
        // already reaches the needed timeline.
        let w = resolve(&ledger(3), 2_000, 30_000, 1_000).unwrap();
        assert_eq!(w.seqs(), vec![0, 1, 2]);
        assert!(w.truncated_front, "caller must be able to warn about a short pre-roll");
    }

    #[test]
    fn fails_when_the_post_roll_has_not_been_written_yet() {
        // Trigger at 2s with 5s post-roll needs the timeline to reach 7s; only 3s exists.
        let err = resolve(&ledger(3), 2_000, 30_000, 5_000).unwrap_err();
        assert!(matches!(err, WindowError::PostRollUnavailable { .. }));
    }

    #[test]
    fn fails_on_an_empty_buffer() {
        let err = resolve(&SegmentLedger::default(), 1_000, 30_000, 5_000).unwrap_err();
        assert!(matches!(err, WindowError::EmptyBuffer));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p localplay-replay window`
Expected: FAIL — `cannot find function resolve`.

- [ ] **Step 3: Implement**

```rust
//! Resolving a trigger instant into the set of segments to concatenate.

use crate::ledger::{Segment, SegmentLedger};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WindowError {
    #[error("the replay buffer is empty; nothing has been captured yet")]
    EmptyBuffer,
    #[error(
        "post-roll not yet on disk: needed capture timeline to reach {needed_ms}ms, \
         but only {available_ms}ms is available"
    )]
    PostRollUnavailable { needed_ms: u64, available_ms: u64 },
}

/// The segments to concatenate. Boundaries are segment-aligned by construction —
/// stream copy can only cut on keyframes, and segment starts are keyframes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentWindow {
    pub segments: Vec<Segment>,
    /// True when the requested pre-roll extends before the start of the buffer.
    pub truncated_front: bool,
}

impl SegmentWindow {
    pub fn seqs(&self) -> Vec<u64> {
        self.segments.iter().map(|s| s.seq).collect()
    }

    /// Nominal duration, accurate to within one segment (spec §6.3).
    pub fn duration_ms(&self) -> u64 {
        self.segments.iter().map(|s| s.duration_ms).sum()
    }
}

/// Resolve `[trigger_ms - pre_ms, trigger_ms + post_ms]` against the ledger.
pub fn resolve(
    ledger: &SegmentLedger,
    trigger_ms: u64,
    pre_ms: u64,
    post_ms: u64,
) -> Result<SegmentWindow, WindowError> {
    if ledger.is_empty() {
        return Err(WindowError::EmptyBuffer);
    }

    let available_ms = ledger.span_ms();
    let needed_ms = trigger_ms + post_ms;
    if available_ms < needed_ms {
        return Err(WindowError::PostRollUnavailable { needed_ms, available_ms });
    }

    let want_start = trigger_ms.saturating_sub(pre_ms);
    let want_end = needed_ms;
    // `saturating_sub` clamps a pre-roll reaching before t=0 down to 0, so compare
    // the *unclamped* request against the buffer's first segment to still detect a
    // short pre-roll. Comparing want_start instead can never report truncation.
    let buffer_start = ledger.segments()[0].start_ms;
    let truncated_front = trigger_ms < pre_ms.saturating_add(buffer_start);

    let segments: Vec<Segment> = ledger
        .segments()
        .iter()
        .filter(|s| s.end_ms() > want_start && s.start_ms < want_end)
        .cloned()
        .collect();

    Ok(SegmentWindow { segments, truncated_front })
}
```

Add `pub mod scanner; pub mod window;` to `crates/replay/src/lib.rs`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p localplay-replay window`
Expected: `test result: ok. 4 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/replay
git commit -m "feat(replay): resolve trigger instants into segment windows"
```

---

## Task 8: Capture and audio traits with non-Windows stubs

**Files:**
- Create: `crates/capture/src/lib.rs`, `crates/capture/src/stub.rs`
- Test: `crates/capture/src/stub.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn stub_emits_the_requested_frame_count_at_the_requested_rate() {
        let cfg = StubConfig { width: 64, height: 48, fps: 60 };
        let mut cap = StubCapture::new(cfg);
        cap.start().unwrap();
        // 0.25s of timeline.
        let frames = cap.drain_for(Duration::from_millis(250));
        assert_eq!(frames.len(), 15, "60fps for 250ms");
    }

    #[test]
    fn stub_frame_pts_are_monotonic_and_frame_sized() {
        let cfg = StubConfig { width: 64, height: 48, fps: 30 };
        let mut cap = StubCapture::new(cfg);
        cap.start().unwrap();
        let frames = cap.drain_for(Duration::from_millis(100));
        assert!(frames.windows(2).all(|w| w[0].pts < w[1].pts), "pts must increase");
        // BGRA8 = 4 bytes per pixel.
        assert_eq!(frames[0].data.len(), 64 * 48 * 4);
    }

    #[test]
    fn stub_audio_emits_silence_in_whole_10ms_blocks() {
        let mut a = StubAudio::new(AudioFormat::default());
        a.start().unwrap();
        let blocks = a.drain_for(Duration::from_millis(50));
        assert_eq!(blocks.len(), 5);
        // 48000Hz / 100 blocks = 480 frames = 960 samples stereo = 1920 bytes.
        assert_eq!(blocks[0].data.len(), 1920);
        assert!(blocks[0].data.iter().all(|b| *b == 0), "stub audio is silence");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p localplay-capture stub`
Expected: FAIL — `cannot find type StubCapture`.

- [ ] **Step 3: Implement the traits and types**

```rust
//! Frame and audio sources.

use std::time::Duration;

pub mod stub;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Windows Graphics Capture hands us BGRA8.
    Bgra8,
}

/// A single captured frame with CPU-visible pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub data: Vec<u8>,
    /// Monotonic, from the same clock as `AudioBuffer::pts` (spec §5.1).
    pub pts: Duration,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl Default for AudioFormat {
    fn default() -> Self {
        // Fixed at 48kHz stereo: the audio timeline must be exactly derivable
        // from the byte count (spec §13).
        Self { sample_rate: 48_000, channels: 2 }
    }
}

impl AudioFormat {
    /// Bytes per 10ms block, the granularity WASAPI loopback delivers.
    pub fn bytes_per_10ms(&self) -> usize {
        (self.sample_rate as usize / 100) * self.channels as usize * 2
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioBuffer {
    /// Interleaved PCM, s16le.
    pub data: Vec<u8>,
    pub frames: usize,
    pub pts: Duration,
    pub format: AudioFormat,
}

pub trait CaptureBackend: Send {
    fn start(&mut self) -> anyhow::Result<()>;
    fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<Option<Frame>>;
    fn stop(&mut self) -> anyhow::Result<()>;
}

pub trait AudioBackend: Send {
    fn start(&mut self) -> anyhow::Result<()>;
    fn next_buffer(&mut self, timeout: Duration) -> anyhow::Result<Option<AudioBuffer>>;
    fn stop(&mut self) -> anyhow::Result<()>;
}
```

```rust
//! Deterministic stand-ins so the whole pipeline is testable off-Windows.

use crate::{AudioBackend, AudioBuffer, AudioFormat, CaptureBackend, Frame, PixelFormat};
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct StubConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Synthetic video: a moving gradient so the encoder sees real inter-frame change.
pub struct StubCapture {
    cfg: StubConfig,
    frame_index: u64,
    started_at: Option<std::time::Instant>,
}

impl StubCapture {
    pub fn new(cfg: StubConfig) -> Self {
        Self { cfg, frame_index: 0, started_at: None }
    }

    /// Produce the frames for `elapsed`, without sleeping.
    pub fn drain_for(&mut self, elapsed: Duration) -> Vec<Frame> {
        let wanted = (elapsed.as_secs_f64() * self.cfg.fps as f64).floor() as u64;
        (0..wanted).map(|_| self.render_next()).collect()
    }

    fn render_next(&mut self) -> Frame {
        let (w, h) = (self.cfg.width, self.cfg.height);
        let phase = (self.frame_index % 256) as u8;
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                data.extend_from_slice(&[
                    (x as u8).wrapping_add(phase), // B
                    (y as u8),                     // G
                    phase,                         // R
                    255,                           // A
                ]);
            }
        }
        let pts = Duration::from_micros(
            self.frame_index * 1_000_000 / self.cfg.fps.max(1) as u64,
        );
        self.frame_index += 1;
        Frame { data, pts, width: w, height: h, format: PixelFormat::Bgra8 }
    }
}

impl CaptureBackend for StubCapture {
    fn start(&mut self) -> anyhow::Result<()> {
        self.frame_index = 0;
        self.started_at = Some(std::time::Instant::now());
        Ok(())
    }

    /// Real-time paced, so the CLI's buffer runs at 1x like a real capture source.
    /// `drain_for` deliberately bypasses pacing to keep tests fast.
    fn next_frame(&mut self, _timeout: Duration) -> anyhow::Result<Option<Frame>> {
        let started = self
            .started_at
            .ok_or_else(|| anyhow::anyhow!("capture not started"))?;
        let due = Duration::from_micros(
            self.frame_index * 1_000_000 / self.cfg.fps.max(1) as u64,
        );
        if let Some(sleep) = due.checked_sub(started.elapsed()) {
            std::thread::sleep(sleep);
        }
        Ok(Some(self.render_next()))
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Synthetic audio: silence, so A/V muxing is exercised without needing real audio.
pub struct StubAudio {
    format: AudioFormat,
    block_index: u64,
    started_at: Option<std::time::Instant>,
}

impl StubAudio {
    pub fn new(format: AudioFormat) -> Self {
        Self { format, block_index: 0, started_at: None }
    }

    pub fn drain_for(&mut self, elapsed: Duration) -> Vec<AudioBuffer> {
        let blocks = (elapsed.as_millis() / 10) as u64;
        (0..blocks).map(|_| self.next_block()).collect()
    }

    fn next_block(&mut self) -> AudioBuffer {
        let bytes = self.format.bytes_per_10ms();
        let pts = Duration::from_millis(self.block_index * 10);
        self.block_index += 1;
        AudioBuffer {
            data: vec![0u8; bytes],
            frames: self.format.sample_rate as usize / 100,
            pts,
            format: self.format,
        }
    }
}

impl AudioBackend for StubAudio {
    fn start(&mut self) -> anyhow::Result<()> {
        self.block_index = 0;
        self.started_at = Some(std::time::Instant::now());
        Ok(())
    }

    fn next_buffer(&mut self, _timeout: Duration) -> anyhow::Result<Option<AudioBuffer>> {
        let started = self
            .started_at
            .ok_or_else(|| anyhow::anyhow!("audio capture not started"))?;
        let due = Duration::from_millis(self.block_index * 10);
        if let Some(sleep) = due.checked_sub(started.elapsed()) {
            std::thread::sleep(sleep);
        }
        Ok(Some(self.next_block()))
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p localplay-capture stub`
Expected: `test result: ok. 3 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/capture
git commit -m "feat(capture): capture/audio traits with deterministic stub backends"
```

---

## Task 9: `FfmpegEncoder` — one child, two pipe inputs

**Files:**
- Create: `crates/encoder/src/lib.rs`, `crates/encoder/src/ffmpeg.rs`
- Test: `crates/encoder/tests/segmenting.rs`

- [ ] **Step 1: Write the failing test**

```rust
//! Drives the encoder with stub sources and asserts it produces segments
//! containing BOTH a video and an audio stream.
use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_encoder::{EncodeConfig, FfmpegEncoder, Encoder, VideoCodec};
use localplay_media::FfmpegBinaries;
use std::time::Duration;

#[test]
fn produces_segments_with_both_a_video_and_an_audio_stream() {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().unwrap();

    let cfg = EncodeConfig::for_tests_software(
        VideoCodec::H264,
        64,
        48,
        30,
        dir.path().to_path_buf(),
        1, // 1s segments
    );

    let mut enc = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn encoder");
    let mut video = StubCapture::new(StubConfig { width: 64, height: 48, fps: 30 });
    let mut audio = StubAudio::new(AudioFormat::default());
    video.start().unwrap();
    audio.start().unwrap();

    // 3 seconds of timeline => at least 2 completed segments.
    for frame in video.drain_for(Duration::from_secs(3)) {
        enc.submit_video(&frame).expect("submit video");
    }
    for block in audio.drain_for(Duration::from_secs(3)) {
        enc.submit_audio(&block).expect("submit audio");
    }
    enc.finish().expect("flush encoder");

    let mut segments: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "mp4"))
        .collect();
    segments.sort();
    assert!(segments.len() >= 2, "expected >=2 segments, got {}", segments.len());

    let info = localplay_media::MediaInfo::probe(&bin, &segments[0]).unwrap();
    assert!(info.video.is_some(), "segment must contain video");
    assert!(info.audio.is_some(), "segment must contain audio");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p localplay-encoder --features test-encoders --test segmenting`
Expected: FAIL — `cannot find type FfmpegEncoder`.

- [ ] **Step 3: Implement the trait surface**

```rust
//! Hardware video encoding through one long-lived ffmpeg child process.

use localplay_capture::{AudioBuffer, Frame};
use std::path::PathBuf;

pub mod ffmpeg;

pub use ffmpeg::FfmpegEncoder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
}

impl VideoCodec {
    pub fn hw_encoder_name(self, vendor: Vendor) -> &'static str {
        match (self, vendor) {
            (VideoCodec::H264, Vendor::Nvenc) => "h264_nvenc",
            (VideoCodec::H264, Vendor::Qsv) => "h264_qsv",
            (VideoCodec::H264, Vendor::Amf) => "h264_amf",
            (VideoCodec::Hevc, Vendor::Nvenc) => "hevc_nvenc",
            (VideoCodec::Hevc, Vendor::Qsv) => "hevc_qsv",
            (VideoCodec::Hevc, Vendor::Amf) => "hevc_amf",
        }
    }
}

/// GPU encoder vendors. There is deliberately no software variant: a silent CPU
/// fallback would violate principle 3 (spec §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Nvenc,
    Qsv,
    Amf,
}

#[derive(Debug, Clone)]
pub struct EncodeConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub segment_ms: u64,
    pub scratch_dir: PathBuf,
    pub audio_bitrate_kbps: u32,
    /// `None` means "real hardware encoder" — the only shipping configuration.
    pub(crate) software_encoder: Option<&'static str>,
}

impl EncodeConfig {
    pub fn hardware(
        codec: VideoCodec,
        vendor: Vendor,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        segment_ms: u64,
        scratch_dir: PathBuf,
    ) -> Self {
        Self {
            codec,
            width,
            height,
            fps,
            bitrate_kbps,
            segment_ms,
            scratch_dir,
            audio_bitrate_kbps: 192,
            software_encoder: Some(codec.hw_encoder_name(vendor)),
        }
    }

    /// Only available to tests and dev builds. The CLI cannot construct this from
    /// a user config file, which is what keeps "no CPU fallback" honest.
    #[cfg(any(test, feature = "test-encoders"))]
    pub fn for_tests_software(
        codec: VideoCodec,
        width: u32,
        height: u32,
        fps: u32,
        scratch_dir: PathBuf,
        segment_ms: u64,
    ) -> Self {
        Self {
            codec,
            width,
            height,
            fps,
            bitrate_kbps: 2_000,
            segment_ms,
            scratch_dir,
            audio_bitrate_kbps: 128,
            software_encoder: None,
        }
    }

    pub(crate) fn encoder_name(&self) -> &'static str {
        self.software_encoder.unwrap_or("libx264")
    }
}

pub trait Encoder: Send {
    fn submit_video(&mut self, frame: &Frame) -> anyhow::Result<()>;
    fn submit_audio(&mut self, audio: &AudioBuffer) -> anyhow::Result<()>;
    fn finish(&mut self) -> anyhow::Result<()>;
    /// Codec actually in use, for ffprobe assertions and the UI.
    fn active_encoder(&self) -> &'static str;
}
```

- [ ] **Step 4: Implement the ffmpeg child**

```rust
//! One ffmpeg process, two pipe inputs, segmented MP4 output.
//!
//! Video arrives on `pipe:0` (stdin). Audio arrives on `pipe:1` (stdout) — we write
//! into the child's stdout. This avoids needing an extra inherited descriptor, which
//! is awkward to set up portably with `std::process`.

use crate::{EncodeConfig, Encoder};
use anyhow::{bail, Context, Result};
use localplay_capture::{AudioBuffer, Frame};
use localplay_media::FfmpegBinaries;
use std::io::Write;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub struct FfmpegEncoder {
    child: Child,
    video_in: ChildStdin,
    audio_in: ChildStdout,
    encoder_name: &'static str,
}

impl FfmpegEncoder {
    pub fn spawn(bin: &FfmpegBinaries, cfg: &EncodeConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.scratch_dir)
            .with_context(|| format!("creating {}", cfg.scratch_dir.display()))?;

        let pattern = cfg.scratch_dir.join("seg-%06d.mp4");
        let keyframe_secs = cfg.segment_ms as f64 / 1000.0;
        let gop = (cfg.fps as f64 * keyframe_secs).round().max(1.0) as u32;

        let mut cmd = Command::new(&bin.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            // Video input: raw BGRA frames on stdin.
            .args(["-f", "rawvideo", "-pix_fmt", "bgra"])
            .args(["-s", &format!("{}x{}", cfg.width, cfg.height)])
            .args(["-r", &cfg.fps.to_string()])
            .args(["-i", "pipe:0"])
            // Audio input: raw s16le PCM on the child's stdout.
            .args(["-f", "s16le", "-ar", "48000", "-ac", "2", "-i", "pipe:1"])
            .args(["-c:v", cfg.encoder_name()])
            .args(["-b:v", &format!("{}k", cfg.bitrate_kbps)])
            .args(["-g", &gop.to_string()])
            // Forced keyframes are what make segment boundaries cuttable (spec §6.3).
            .args([
                "-force_key_frames",
                &format!("expr:gte(t,n_forced*{keyframe_secs})"),
            ])
            .args(["-c:a", "aac", "-b:a", &format!("{}k", cfg.audio_bitrate_kbps)])
            .args(["-f", "segment"])
            .args(["-segment_time", &keyframe_secs.to_string()])
            .args(["-segment_format", "mp4"])
            .args(["-reset_timestamps", "1"])
            .arg(&pattern)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "spawning {} (requested encoder: {})",
                bin.ffmpeg.display(),
                cfg.encoder_name()
            )
        })?;

        let video_in = child.stdin.take().context("child stdin unavailable")?;
        let audio_in = child.stdout.take().context("child stdout unavailable")?;

        Ok(Self {
            child,
            video_in,
            audio_in,
            encoder_name: cfg.encoder_name(),
        })
    }
}

impl Encoder for FfmpegEncoder {
    fn submit_video(&mut self, frame: &Frame) -> Result<()> {
        self.video_in.write_all(&frame.data).context("writing video frame")?;
        Ok(())
    }

    fn submit_audio(&mut self, audio: &AudioBuffer) -> Result<()> {
        self.audio_in.write_all(&audio.data).context("writing audio block")?;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        // Closing stdin ends the video input; closing stdout ends the audio input.
        self.video_in.flush().ok();
        self.audio_in.flush().ok();

        let status = self.child.wait().context("waiting for ffmpeg")?;
        if !status.success() {
            let mut stderr = String::new();
            if let Some(mut e) = self.child.stderr.take() {
                use std::io::Read;
                let _ = e.read_to_string(&mut stderr);
            }
            bail!(
                "ffmpeg exited with {status} using encoder '{}': {}",
                self.encoder_name,
                stderr.trim()
            );
        }
        Ok(())
    }

    fn active_encoder(&self) -> &'static str {
        self.encoder_name
    }
}
```

Add `crates/encoder/src/probe.rs` — this is criterion 7's implementation, and it is
what makes "no silent CPU fallback" real rather than aspirational:

```rust
//! Choosing a hardware encoder, and failing loudly when there isn't one.

use crate::{VideoCodec, Vendor};
use anyhow::{bail, Context, Result};
use localplay_media::FfmpegBinaries;
use std::process::Command;

/// Vendors tried, in order, for `vendor = "auto"`.
const AUTO_ORDER: [Vendor; 3] = [Vendor::Nvenc, Vendor::Qsv, Vendor::Amf];

/// Pick a hardware encoder, or explain precisely why none is usable.
///
/// There is no software fallback (spec §3.2). A CPU encode would silently destroy
/// in-game performance, so failing loudly is the correct behaviour.
pub fn select_vendor(bin: &FfmpegBinaries, requested: &str, codec: VideoCodec) -> Result<Vendor> {
    // Resolve the request before touching ffmpeg, so a bad config value fails
    // immediately and does not depend on the machine's hardware.
    let candidates: Vec<Vendor> = match requested {
        "auto" => AUTO_ORDER.to_vec(),
        "nvenc" => vec![Vendor::Nvenc],
        "qsv" => vec![Vendor::Qsv],
        "amf" => vec![Vendor::Amf],
        other => bail!("unknown encode.vendor: {other}"),
    };

    let advertised = advertised_encoders(bin)?;
    if let Some(vendor) = candidates
        .iter()
        .find(|v| advertised.iter().any(|e| *e == codec.hw_encoder_name(**v)))
    {
        return Ok(*vendor);
    }

    let wanted: Vec<&str> = candidates.iter().map(|v| codec.hw_encoder_name(*v)).collect();
    bail!(
        "no usable hardware encoder. ffmpeg advertises none of: {}. \
         Install the GPU vendor runtime (NVIDIA driver / Intel graphics driver / \
         AMD Adrenalin), or set encode.vendor to a vendor this machine has. \
         localplay will not fall back to CPU encoding because it would cost game performance.",
        wanted.join(", ")
    )
}

/// Encoder names ffmpeg advertises. `-encoders` lines look like:
/// ` V....D h264_nvenc  NVIDIA NVENC H.264 encoder (codec h264)`.
fn advertised_encoders(bin: &FfmpegBinaries) -> Result<Vec<String>> {
    let out = Command::new(&bin.ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .with_context(|| format!("running {}", bin.ffmpeg.display()))?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_vendor_name_fails_without_probing_ffmpeg() {
        // A bogus binary path proves validation happens before any spawn.
        let bin = FfmpegBinaries {
            ffmpeg: "/nonexistent/ffmpeg".into(),
            ffprobe: "/nonexistent/ffprobe".into(),
        };
        let err = select_vendor(&bin, "voodoo", VideoCodec::H264).unwrap_err();
        assert!(err.to_string().contains("voodoo"), "got: {err}");
    }
}
```

Add `pub mod probe;` to `crates/encoder/src/lib.rs`, and `pub use probe::select_vendor;`.

> The `xtask probe` task (Task 16) prints the raw `-encoders` listing for debugging;
> this module is the programmatic selection the CLI actually uses.

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p localplay-encoder --features test-encoders --test segmenting`
Expected: `test result: ok. 1 passed`.

> If this hangs, ffmpeg is blocking on the audio input. Confirm the child's stdout
> is being written to and that `-nostdin` is present.

- [ ] **Step 6: Commit**

```bash
git add crates/encoder
git commit -m "feat(encoder): single ffmpeg child with video and audio pipe inputs"
```

---

## Task 10: `RingBuffer` and `ClipSplicer`

**Files:**
- Create: `crates/replay/src/splice.rs`, `crates/replay/src/buffer.rs`
- Modify: `crates/replay/src/lib.rs`
- Test: `crates/replay/tests/end_to_end_clip.rs`

- [ ] **Step 1: Write the failing test**

```rust
//! The PoC's core loop, exercised end to end off-Windows with stub sources.
use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_encoder::{EncodeConfig, FfmpegEncoder, VideoCodec};
use localplay_media::{FfmpegBinaries, MediaInfo};
use localplay_replay::buffer::{BufferConfig, RingBuffer};
use std::time::Duration;

#[test]
fn triggering_produces_a_clip_with_video_and_audio_and_stays_under_the_cap() {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let scratch = tempfile::tempdir().unwrap();
    let clips = tempfile::tempdir().unwrap();

    let encode = EncodeConfig::for_tests_software(
        VideoCodec::H264,
        64,
        48,
        10,
        scratch.path().to_path_buf(),
        1, // 1s segments
    );
    let cfg = BufferConfig {
        pre_ms: 3_000,
        post_ms: 1_000,
        scratch_cap_bytes: 1_000_000,
        segment_ms: 1_000,
        clips_dir: clips.path().to_path_buf(),
    };

    let mut video = StubCapture::new(StubConfig { width: 64, height: 48, fps: 10 });
    let mut audio = StubAudio::new(AudioFormat::default());
    let mut encoder = FfmpegEncoder::spawn(&bin, &encode).unwrap();
    let mut ring = RingBuffer::start(
        &bin,
        cfg,
        scratch.path().to_path_buf(),
        "libx264".to_string(),
    )
    .expect("start ring buffer");

    // 6 seconds of timeline: segments 0..5 exist, so 0..4 are complete.
    for frame in video.drain_for(Duration::from_secs(6)) {
        encoder.submit_video(&frame).unwrap();
    }
    for block in audio.drain_for(Duration::from_secs(6)) {
        encoder.submit_audio(&block).unwrap();
    }
    encoder.finish().unwrap();

    ring.scan_once().expect("scan scratch dir");
    assert!(ring.stats().segments >= 4, "expected completed segments, got {}", ring.stats().segments);
    assert!(
        ring.stats().bytes_on_disk <= cfg.scratch_cap_bytes,
        "scratch exceeded its cap"
    );

    let clip = ring.trigger(4_000, "test-clip").expect("trigger");
    let info = MediaInfo::probe(&bin, &clip.path).unwrap();

    assert!(info.video.is_some(), "clip must have video");
    assert!(info.audio.is_some(), "clip must have audio");
    // pre=3s post=1s => 4s of timeline, segment-aligned so >= 3s.
    assert!(
        (3_000..=4_500).contains(&info.duration_ms),
        "clip duration {}ms unexpected",
        info.duration_ms
    );
    assert_eq!(clip.encoder, "libx264");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p localplay-replay --features test-encoders --test end_to_end_clip`
Expected: FAIL — `cannot find type RingBuffer`.

- [ ] **Step 3: Implement `ClipSplicer`**

```rust
//! Turning a segment window into a single lossless clip file.

use crate::window::SegmentWindow;
use anyhow::{bail, Context, Result};
use localplay_media::{edit, FfmpegBinaries};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ClipMetadata {
    pub path: PathBuf,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub encoder: String,
}

pub struct ClipSplicer;

impl ClipSplicer {
    /// Concatenate whole segments with `-c copy`.
    ///
    /// No leading-segment trim: segment boundaries are keyframes, so `-ss`/`-c copy`
    /// would resolve to the same frame and change nothing (see plan refinements).
    pub fn splice(
        bin: &FfmpegBinaries,
        window: &SegmentWindow,
        out: &Path,
        encoder: &str,
    ) -> Result<ClipMetadata> {
        if window.segments.is_empty() {
            bail!("cannot splice an empty segment window");
        }

        let list = out.with_extension("concat.txt");
        {
            let mut f = std::fs::File::create(&list)
                .with_context(|| format!("creating {}", list.display()))?;
            for seg in &window.segments {
                // The concat demuxer needs escaped, absolute-ish paths.
                let p = std::fs::canonicalize(&seg.file)
                    .with_context(|| format!("resolving {}", seg.file.display()))?;
                writeln!(f, "file '{}'", p.display().to_string().replace('\'', "'\\''"))?;
            }
        }

        edit::concat_lossless(bin, &list, out)?;
        let _ = std::fs::remove_file(&list);

        let info = localplay_media::MediaInfo::probe(bin, out)?;
        Ok(ClipMetadata {
            path: out.to_path_buf(),
            duration_ms: info.duration_ms,
            size_bytes: info.size_bytes,
            encoder: encoder.to_string(),
        })
    }
}
```

- [ ] **Step 4: Implement `RingBuffer`**

```rust
//! The replay ring: scratch directory, ledger, eviction, and clip extraction.

use crate::ledger::{Segment, SegmentLedger};
use crate::scanner::newly_complete;
use crate::splice::{ClipMetadata, ClipSplicer};
use crate::window::{self, WindowError};
use anyhow::{Context, Result};
use localplay_media::FfmpegBinaries;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct BufferConfig {
    pub pre_ms: u64,
    pub post_ms: u64,
    pub scratch_cap_bytes: u64,
    pub segment_ms: u64,
    pub clips_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferStats {
    pub segments: usize,
    pub bytes_on_disk: u64,
    pub span_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum TriggerError {
    #[error(transparent)]
    Window(#[from] WindowError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub struct RingBuffer {
    cfg: BufferConfig,
    bin: FfmpegBinaries,
    scratch_dir: PathBuf,
    ledger: SegmentLedger,
    highest_known: Option<u64>,
    /// Name of the encoder actually in use, recorded on every clip so an ffprobe
    /// mismatch is detectable (spec §11 criterion 5).
    encoder: String,
}

impl RingBuffer {
    pub fn start(
        bin: &FfmpegBinaries,
        cfg: BufferConfig,
        scratch_dir: PathBuf,
        encoder: String,
    ) -> Result<Self> {
        std::fs::create_dir_all(&scratch_dir)
            .with_context(|| format!("creating {}", scratch_dir.display()))?;
        std::fs::create_dir_all(&cfg.clips_dir)
            .with_context(|| format!("creating {}", cfg.clips_dir.display()))?;
        Ok(Self {
            cfg,
            bin: bin.clone(),
            scratch_dir,
            ledger: SegmentLedger::default(),
            highest_known: None,
            encoder,
        })
    }

    /// Adopt segments already on disk from a previous run.
    pub fn adopt_existing(&mut self) -> Result<usize> {
        let before = self.ledger.len();
        self.scan_once()?;
        Ok(self.ledger.len() - before)
    }

    /// Find newly completed segments, index them, and evict to the cap.
    pub fn scan_once(&mut self) -> Result<()> {
        let observed = self.observed_seqs()?;
        for seq in newly_complete(&observed, self.highest_known) {
            let file = self.segment_path(seq);
            let bytes = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            self.ledger.push(Segment {
                seq,
                file,
                start_ms: seq * self.cfg.segment_ms,
                duration_ms: self.cfg.segment_ms,
                bytes,
            });
            self.highest_known = Some(seq);
        }
        self.enforce_cap()?;
        Ok(())
    }

    /// Delete oldest segments until under the cap. Deleting in Rust rather than
    /// using `-segment_wrap` keeps the ledger authoritative.
    fn enforce_cap(&mut self) -> Result<()> {
        for seg in self.ledger.evict_to_cap(self.cfg.scratch_cap_bytes) {
            if let Err(e) = std::fs::remove_file(&seg.file) {
                tracing::warn!("could not evict {}: {e}", seg.file.display());
            }
        }
        Ok(())
    }

    fn observed_seqs(&self) -> Result<Vec<u64>> {
        let mut seqs = Vec::new();
        for entry in std::fs::read_dir(&self.scratch_dir)
            .with_context(|| format!("reading {}", self.scratch_dir.display()))?
        {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix("seg-") {
                if let Some(num) = rest.strip_suffix(".mp4") {
                    if let Ok(seq) = num.parse::<u64>() {
                        seqs.push(seq);
                    }
                }
            }
        }
        Ok(seqs)
    }

    fn segment_path(&self, seq: u64) -> PathBuf {
        self.scratch_dir.join(format!("seg-{seq:06}.mp4"))
    }

    pub fn stats(&self) -> BufferStats {
        BufferStats {
            segments: self.ledger.len(),
            bytes_on_disk: self.ledger.total_bytes(),
            span_ms: self.ledger.span_ms(),
        }
    }

    /// Persist the ledger so the index survives a crash (spec §6.4).
    pub fn save_ledger(&self) -> Result<()> {
        self.ledger.save_atomic(&self.scratch_dir.join("ledger.toml"))
    }

    /// Extract a clip around `trigger_ms` on the capture timeline.
    ///
    /// Caller must ensure the post-roll has been written; `resolve` returns
    /// `PostRollUnavailable` otherwise.
    pub fn trigger(&self, trigger_ms: u64, stem: &str) -> Result<ClipMetadata, TriggerError> {
        let win = window::resolve(
            &self.ledger,
            trigger_ms,
            self.cfg.pre_ms,
            self.cfg.post_ms,
        )?;
        if win.truncated_front {
            tracing::warn!(
                "only {}ms of pre-roll was buffered for a {}ms request",
                win.segments.first().map(|_| win.duration_ms()).unwrap_or(0),
                self.cfg.pre_ms
            );
        }
        let out = self.cfg.clips_dir.join(format!("{stem}.mp4"));
        let meta = ClipSplicer::splice(&self.bin, &win, &out, &self.encoder)?;
        Ok(meta)
    }
}
```

> `save_ledger`'s path and `RingBuffer::start`'s extra parameter are the only
> signature differences from spec §5.3; the spec sketched the shape, not the exact
> arguments.

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p localplay-replay --features test-encoders --test end_to_end_clip`
Expected: `test result: ok. 1 passed`.

- [ ] **Step 6: Commit**

```bash
git add crates/replay
git commit -m "feat(replay): ring buffer with eviction and lossless clip splicing"
```

---

## Task 11: SQLite store

**Files:**
- Create: `crates/store/src/lib.rs`
- Test: `crates/store/src/lib.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrating_is_idempotent() {
        let s = Store::open_in_memory().unwrap();
        // Migrate first: reading the version before migrating yields 0, so
        // comparing against it afterwards can never hold.
        s.migrate().unwrap();
        let v1 = s.schema_version().unwrap();
        s.migrate().unwrap();
        assert_eq!(s.schema_version().unwrap(), v1, "re-migrating must not bump the version");
    }

    #[test]
    fn inserts_and_lists_clips_newest_first() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let a = s.insert_clip(&NewClip {
            path: "/clips/a.mp4".into(),
            started_at_ms: 1_000,
            duration_ms: 4_000,
            size_bytes: 100,
            codec: "h264".into(),
        }).unwrap();
        let _b = s.insert_clip(&NewClip {
            path: "/clips/b.mp4".into(),
            started_at_ms: 2_000,
            duration_ms: 4_000,
            size_bytes: 200,
            codec: "h264".into(),
        }).unwrap();

        let clips = s.list_clips().unwrap();
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].path, PathBuf::from("/clips/b.mp4"), "newest first");
        assert_eq!(clips[1].id, a);
    }

    #[test]
    fn rejects_a_duplicate_clip_path() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let clip = NewClip {
            path: "/clips/dup.mp4".into(),
            started_at_ms: 1_000,
            duration_ms: 1_000,
            size_bytes: 1,
            codec: "h264".into(),
        };
        s.insert_clip(&clip).unwrap();
        assert!(s.insert_clip(&clip).is_err(), "path is UNIQUE");
    }

    #[test]
    fn delete_before_unlink_ordering_is_exposed_to_the_caller() {
        // The store must be able to delete the row and hand back the path, so the
        // caller can unlink afterwards. Spec §8.2: no file is deleted without a
        // committed row naming it.
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let id = s.insert_clip(&NewClip {
            path: "/clips/c.mp4".into(),
            started_at_ms: 1_000,
            duration_ms: 1_000,
            size_bytes: 1,
            codec: "h264".into(),
        }).unwrap();

        let removed = s.delete_clip_returning_path(id).unwrap();
        assert_eq!(removed.as_deref(), Some("/clips/c.mp4"));
        assert_eq!(s.delete_clip_returning_path(id).unwrap(), None, "second delete is a no-op");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p localplay-store`
Expected: FAIL — `cannot find type Store`.

- [ ] **Step 3: Implement**

```rust
//! SQLite index of clips and sessions. Schema mirrors spec §5.5.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: i32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewClip {
    pub path: PathBuf,
    pub started_at_ms: u64,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub codec: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    pub id: i64,
    pub path: PathBuf,
    pub started_at_ms: u64,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub codec: String,
    pub favourite: bool,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self { conn: Connection::open_in_memory().context("opening in-memory db")? })
    }

    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self { conn: Connection::open(path).with_context(|| format!("opening {}", path.display()))? })
    }

    /// Forward-only migrations. A schema newer than this build is refused rather
    /// than silently downgraded.
    pub fn migrate(&self) -> Result<()> {
        let current: i32 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current > SCHEMA_VERSION {
            anyhow::bail!(
                "database schema v{current} is newer than this build supports (v{SCHEMA_VERSION}); \
                 refusing to downgrade"
            );
        }
        if current == SCHEMA_VERSION {
            return Ok(());
        }

        self.conn.execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS sessions (
                id          INTEGER PRIMARY KEY,
                game        TEXT,
                started_at  INTEGER NOT NULL,
                ended_at    INTEGER,
                scratch_dir TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS clips (
                id          INTEGER PRIMARY KEY,
                session_id  INTEGER REFERENCES sessions(id),
                path        TEXT NOT NULL UNIQUE,
                started_at  INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                size_bytes  INTEGER NOT NULL,
                codec       TEXT NOT NULL,
                favourite   INTEGER NOT NULL DEFAULT 0,
                created_at  INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS events (
                id         INTEGER PRIMARY KEY,
                session_id INTEGER REFERENCES sessions(id),
                kind       TEXT NOT NULL,
                at         INTEGER NOT NULL,
                payload    TEXT,
                clip_id    INTEGER REFERENCES clips(id)
            );
            CREATE INDEX IF NOT EXISTS idx_clips_started_at ON clips(started_at);
            CREATE INDEX IF NOT EXISTS idx_events_session   ON events(session_id, at);
            "#,
        )?;
        self.conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i32> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    pub fn insert_clip(&self, clip: &NewClip) -> Result<i64> {
        let path = clip.path.to_string_lossy();
        self.conn.execute(
            "INSERT INTO clips (path, started_at, duration_ms, size_bytes, codec, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                path,
                clip.started_at_ms as i64,
                clip.duration_ms as i64,
                clip.size_bytes as i64,
                clip.codec,
                now_ms(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn list_clips(&self) -> Result<Vec<Clip>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, started_at, duration_ms, size_bytes, codec, favourite
             FROM clips ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Clip {
                id: r.get(0)?,
                path: PathBuf::from(r.get::<_, String>(1)?),
                started_at_ms: r.get::<_, i64>(2)? as u64,
                duration_ms: r.get::<_, i64>(3)? as u64,
                size_bytes: r.get::<_, i64>(4)? as u64,
                codec: r.get(5)?,
                favourite: r.get::<_, i64>(6)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Delete the row and return its path so the caller can unlink *afterwards*
    /// (spec §8.2). Returns `None` if no such row existed.
    pub fn delete_clip_returning_path(&self, id: i64) -> Result<Option<String>> {
        let path: Option<String> = self
            .conn
            .query_row("SELECT path FROM clips WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?;
        if path.is_some() {
            self.conn.execute("DELETE FROM clips WHERE id = ?1", params![id])?;
        }
        Ok(path)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
```

Add to `crates/store/Cargo.toml`:

```toml
[dependencies]
rusqlite = { version = "0.32", features = ["bundled"] }
```

> If `0.32` does not resolve, run `cargo add rusqlite --features bundled -p localplay-store`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p localplay-store`
Expected: `test result: ok. 4 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/store
git commit -m "feat(store): sqlite schema, clip insert/list, delete-before-unlink ordering"
```

---

## Task 12: Trigger type, loopback enforcement, and the Windows hotkey

**Files:**
- Create: `crates/events/src/lib.rs`, `crates/events/src/hotkey.rs`
- Test: `crates/events/tests/no_egress.rs`

- [ ] **Step 1: Write the failing test**

```rust
//! Spec §7.3: enforce "zero egress" by inspecting the source tree.
use std::path::Path;

#[test]
fn no_http_client_is_constructed_outside_the_loopback_module() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut offenders = Vec::new();

    for entry in walk_rs(&root) {
        let rel = entry.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        if rel.contains("/target/") {
            continue;
        }
        // The LoL module is the one place allowed to build its own client,
        // because it alone talks to 127.0.0.1:2999 with a self-signed cert.
        if rel.ends_with("lol.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&entry).unwrap_or_default();
        if text.contains("reqwest::Client") || text.contains("ureq::agent") {
            offenders.push(rel);
        }
    }

    assert!(
        offenders.is_empty(),
        "outbound HTTP client constructed outside the loopback module: {offenders:?}"
    );
}

#[test]
fn the_gsi_listener_binds_loopback_only() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for entry in walk_rs(&root) {
        let rel = entry.to_string_lossy().replace('\\', "/");
        if rel.contains("/target/") {
            continue;
        }
        let text = std::fs::read_to_string(&entry).unwrap_or_default();
        if text.contains("TcpListener::bind") {
            assert!(
                text.contains("127.0.0.1") || text.contains("LOOPBACK"),
                "{} binds a listener without pinning loopback",
                entry.display()
            );
        }
    }
}

fn walk_rs(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for e in entries.filter_map(|e| e.ok()) {
        let p = e.path();
        // Prune build output and VCS metadata: `target/` holds the whole crates.io
        // registry cache, whose vendored sources would otherwise be walked (and
        // could trip the assertions) on every run.
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
        if p.is_dir() && (name == "target" || name == ".git") {
            continue;
        }
        if p.is_dir() {
            out.extend(walk_rs(&p));
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
    out
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p localplay-events --test no_egress`
Expected: FAIL — the crate has no `src/lib.rs` yet, so the test target cannot build.

- [ ] **Step 3: Implement the crate**

```rust
//! Clip triggers and local-only game event sources.

use std::time::{Duration, Instant};

pub mod hotkey;

/// A request to clip. Both manual and automatic paths funnel through this, so the
/// replay buffer has exactly one entry point (spec §5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    Hotkey,
    GameEvent { kind: EventKind, at: Instant },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Kill,
    Death,
    Assist,
    Objective,
    GameStart,
    GameEnd,
}

/// Maps an instant onto the capture timeline, which is what the ledger indexes.
#[derive(Debug)]
pub struct CaptureClock {
    start: Instant,
}

impl CaptureClock {
    pub fn new() -> Self {
        Self { start: Instant::now() }
    }

    pub fn ms_at(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.start).as_millis() as u64
    }

    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

impl Default for CaptureClock {
    fn default() -> Self {
        Self::new()
    }
}
```

```rust
//! Global hotkey listener via `RegisterHotKey`.

use anyhow::Result;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

/// Key chord to listen for. Parsed from config (spec §10, `[hotkeys] clip`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Virtual-key code, e.g. `0x77` for F8.
    pub vk: u32,
}

impl Hotkey {
    /// Accepts the `"Ctrl+F8"` form used in `config.example.toml`.
    pub fn parse(spec: &str) -> Result<Self> {
        let mut hk = Self { ctrl: false, alt: false, shift: false, vk: 0 };
        let mut key = None;
        for part in spec.split('+') {
            match part.trim().to_ascii_lowercase().as_str() {
                "ctrl" => hk.ctrl = true,
                "alt" => hk.alt = true,
                "shift" => hk.shift = true,
                other => key = Some(other.to_string()),
            }
        }
        hk.vk = parse_key(key.as_deref())?;
        Ok(hk)
    }
}

fn parse_key(key: Option<&str>) -> Result<u32> {
    let key = key.ok_or_else(|| anyhow::anyhow!("hotkey has no main key (e.g. \"Ctrl+F8\")"))?;
    if let Some(n) = key.strip_prefix('f') {
        let n: u32 = n.parse().map_err(|_| anyhow::anyhow!("bad function key: F{n}"))?;
        if (1..=24).contains(&n) {
            return Ok(0x70 + n - 1); // VK_F1..VK_F24
        }
        anyhow::bail!("function key out of range: F{n}");
    }
    let ch = key.chars().next().ok_or_else(|| anyhow::anyhow!("empty key"))?;
    if ch.is_ascii_alphanumeric() {
        return Ok(ch.to_ascii_uppercase() as u32); // VK_A..VK_Z / VK_0..VK_9
    }
    anyhow::bail!("unsupported key: {key}")
}

/// Start listening. Returns a channel that yields once per press.
#[cfg(windows)]
pub fn listen(hk: Hotkey) -> Result<Receiver<()>> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("localplay-hotkey".into())
        .spawn(move || {
            let mut modifiers = 0u32;
            if hk.ctrl {
                modifiers |= MOD_CONTROL.0;
            }
            if hk.alt {
                modifiers |= MOD_ALT.0;
            }
            if hk.shift {
                modifiers |= MOD_SHIFT.0;
            }
            // SAFETY: called on a dedicated thread that owns the message queue.
            unsafe {
                if RegisterHotKey(None, 1, modifiers, hk.vk).is_err() {
                    return;
                }
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    if msg.message == WM_HOTKEY && tx.send(()).is_err() {
                        break;
                    }
                }
            }
        })?;
    Ok(rx)
}

/// Non-Windows builds have no global hotkey; the caller polls instead.
#[cfg(not(windows))]
pub fn listen(_hk: Hotkey) -> Result<Receiver<()>> {
    let (_tx, rx) = channel();
    Ok(rx)
}

/// Poll helper so both platforms share one call site.
pub fn wait_for_press(rx: &Receiver<()>, timeout: Duration) -> bool {
    rx.recv_timeout(timeout).is_ok()
}
```

Add to `crates/events/Cargo.toml`:

```toml
[target.'cfg(windows)'.dependencies]
windows = { version = "0.58", features = [
    "Win32_Foundation",
    "Win32_UI_Input_KeyboardAndMouse",
    "Win32_UI_WindowsAndMessaging",
] }
```

Also add a stub `crates/events/src/lol.rs` with just a module doc comment, so the
`no_egress` test's allowlist path exists from the start:

```rust
//! League of Legends Live Client polling. The only module permitted to build an
//! HTTP client, and it may only ever address 127.0.0.1:2999 (spec §7.1).
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p localplay-events`
Expected: `test result: ok` — including the `hotkey_parse` unit tests if you added them.

- [ ] **Step 5: Commit**

```bash
git add crates/events
git commit -m "feat(events): trigger types, capture clock, hotkey, zero-egress guard test"
```

---

## Task 13: CLI wiring

**Files:**
- Create: `apps/localplay-cli/src/config.rs`, `apps/localplay-cli/src/main.rs`
- Test: `apps/localplay-cli/src/config.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_example_config() {
        let text = include_str!("../../../config.example.toml");
        let cfg = Config::from_toml(text).expect("config.example.toml must stay valid");
        assert_eq!(cfg.buffer.pre_seconds, 30);
        assert_eq!(cfg.buffer.post_seconds, 5);
        assert!(cfg.audio.enabled);
        assert_eq!(cfg.hotkeys.clip, "Ctrl+F8");
    }

    #[test]
    fn rejects_a_software_encoder_request() {
        let text = include_str!("../../../config.example.toml")
            .replace("vendor = \"auto\"", "vendor = \"software\"");
        let err = Config::from_toml(&text).unwrap_err();
        assert!(
            err.to_string().contains("software"),
            "must explain that CPU encoding is not available: {err}"
        );
    }

    #[test]
    fn rejects_a_gsi_port_above_the_ephemeral_range_start() {
        let text = include_str!("../../../config.example.toml")
            .replace("gsi_port = 45671", "gsi_port = 80");
        let err = Config::from_toml(&text).unwrap_err();
        assert!(err.to_string().contains("gsi_port"), "got: {err}");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p localplay-cli config`
Expected: FAIL — `cannot find type Config`.

- [ ] **Step 3: Implement config**

```rust
//! Root configuration: TOML in the app data directory, never environment variables.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub buffer: BufferSection,
    pub encode: EncodeSection,
    pub audio: AudioSection,
    pub storage: StorageSection,
    pub hotkeys: HotkeySection,
    pub events: EventsSection,
}

#[derive(Debug, Deserialize)]
pub struct BufferSection {
    pub pre_seconds: u64,
    pub post_seconds: u64,
    pub segment_time: u64,
    pub scratch_cap_bytes: u64,
    pub scratch_dir: String,
}

#[derive(Debug, Deserialize)]
pub struct EncodeSection {
    pub vendor: String,
    pub codec: String,
    pub bitrate_kbps: u32,
    pub fps: u32,
    pub output_size: String,
}

#[derive(Debug, Deserialize)]
pub struct AudioSection {
    pub enabled: bool,
    pub source: String,
    pub codec: String,
    pub bitrate_kbps: u32,
}

#[derive(Debug, Deserialize)]
pub struct StorageSection {
    pub clips_dir: String,
    pub max_total_bytes: u64,
    pub max_age_days: u64,
}

#[derive(Debug, Deserialize)]
pub struct HotkeySection {
    pub clip: String,
}

#[derive(Debug, Deserialize)]
pub struct EventsSection {
    pub lol_poll_enabled: bool,
    pub gsi_port: u16,
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Self> {
        let cfg: Self = toml::from_str(text).context("parsing config TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &PathBuf) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::from_toml(&text)
    }

    fn validate(&self) -> Result<()> {
        // Fail closed: there is no CPU encoder to fall back to (spec §3.2).
        match self.encode.vendor.as_str() {
            "auto" | "nvenc" | "qsv" | "amf" => {}
            "software" => bail!(
                "vendor = \"software\" is not supported: localplay requires a GPU hardware \
                 encoder (nvenc, qsv or amf) so that capturing does not cost game performance"
            ),
            other => bail!("unknown encode.vendor: {other}"),
        }
        if !(1024..=65535).contains(&self.events.gsi_port) {
            bail!(
                "events.gsi_port must be between 1024 and 65535, got {}",
                self.events.gsi_port
            );
        }
        if self.buffer.segment_time == 0 {
            bail!("buffer.segment_time must be at least 1 second");
        }
        if self.buffer.post_seconds == 0 {
            bail!("buffer.post_seconds must be at least 1 second");
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Implement the CLI**

```rust
//! localplay CLI — the Phase 1 headless replay-buffer PoC.

mod config;

use anyhow::{bail, Context, Result};
use config::Config;
use localplay_capture::stub::{StubAudio, StubCapture, StubConfig};
use localplay_capture::{AudioBackend, AudioFormat, CaptureBackend};
use localplay_encoder::probe::select_vendor;
use localplay_encoder::{EncodeConfig, Encoder, FfmpegEncoder, VideoCodec};
use localplay_events::hotkey::Hotkey;
use localplay_media::FfmpegBinaries;
use localplay_replay::buffer::{BufferConfig, RingBuffer};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    match std::env::args().nth(1).as_deref() {
        Some("buffer") => run_buffer(),
        Some(other) => bail!("unknown subcommand: {other}\nusage: localplay-cli buffer"),
        None => bail!("usage: localplay-cli buffer"),
    }
}

fn app_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("localplay")
}

fn run_buffer() -> Result<()> {
    let app_dir = app_data_dir();
    let cfg_path = app_dir.join("config.toml");
    let cfg = if cfg_path.is_file() {
        Config::load(&cfg_path)?
    } else {
        Config::from_toml(include_str!("../../../config.example.toml"))?
    };

    let bin = FfmpegBinaries::discover(None)?;
    let hotkey = Hotkey::parse(&cfg.hotkeys.clip)?;

    let scratch_dir = if cfg.buffer.scratch_dir.is_empty() {
        app_dir.join("scratch")
    } else {
        PathBuf::from(&cfg.buffer.scratch_dir)
    };
    let clips_dir = if cfg.storage.clips_dir.is_empty() {
        app_dir.join("clips")
    } else {
        PathBuf::from(&cfg.storage.clips_dir)
    };

    let buffer_cfg = BufferConfig {
        pre_ms: cfg.buffer.pre_seconds * 1000,
        post_ms: cfg.buffer.post_seconds * 1000,
        scratch_cap_bytes: cfg.buffer.scratch_cap_bytes,
        segment_ms: cfg.buffer.segment_time * 1000,
        clips_dir: clips_dir.clone(),
    };

    // `--dev-software-encoder` only exists when built with the test-encoders feature.
    let dev_software = std::env::args().any(|a| a == "--dev-software-encoder");
    let (mut encoder, encoder_name) = build_encoder(&bin, &cfg, &scratch_dir, dev_software)?;
    tracing::info!("encoding with {encoder_name}");

    // Phase 1 wires the stub sources so the pipeline runs on any platform. Tasks 14/15
    // swap these for WgcCapture + WasapiLoopback behind the same traits.
    let mut ring = RingBuffer::start(
        &bin,
        buffer_cfg.clone(),
        scratch_dir.clone(),
        encoder_name.clone(),
    )?;
    let adopted = ring.adopt_existing()?;
    if adopted > 0 {
        tracing::info!("adopted {adopted} segments from a previous run");
    }

    let (width, height) = parse_output_size(&cfg.encode.output_size, 1920, 1080);
    let mut capture: Box<dyn CaptureBackend> = Box::new(StubCapture::new(StubConfig {
        width,
        height,
        fps: cfg.encode.fps,
    }));
    let mut audio: Box<dyn AudioBackend> = Box::new(StubAudio::new(AudioFormat::default()));
    capture.start()?;
    audio.start()?;

    let hotkeys = localplay_events::hotkey::listen(hotkey)?;
    let clock = localplay_events::CaptureClock::new();
    tracing::info!(
        "buffering {}s pre / {}s post at {}fps; press {} to clip",
        cfg.buffer.pre_seconds,
        cfg.buffer.post_seconds,
        cfg.encode.fps,
        cfg.hotkeys.clip
    );

    // Capture loop. Frames go to the encoder; the ring scans for completed segments.
    let mut last_scan = Instant::now();
    loop {
        if let Some(frame) = capture.next_frame(Duration::from_millis(5))? {
            encoder.submit_video(&frame)?;
        }
        if let Some(block) = audio.next_buffer(Duration::from_millis(5))? {
            encoder.submit_audio(&block)?;
        }

        if last_scan.elapsed() >= Duration::from_millis(200) {
            ring.scan_once().context("scanning scratch")?;
            ring.save_ledger()?;
            last_scan = Instant::now();

            let stats = ring.stats();
            tracing::debug!(
                "segments={} bytes={} span={}ms",
                stats.segments,
                stats.bytes_on_disk,
                stats.span_ms
            );
            if stats.bytes_on_disk > cfg.buffer.scratch_cap_bytes {
                bail!(
                    "scratch cap violated: {} bytes on disk exceeds {}",
                    stats.bytes_on_disk,
                    cfg.buffer.scratch_cap_bytes
                );
            }
        }

        if localplay_events::hotkey::wait_for_press(&hotkeys, Duration::from_millis(10)) {
            let trigger_ms = clock.ms_at(Instant::now());
            tracing::info!("hotkey pressed at {trigger_ms}ms; waiting for post-roll");

            // Wait for the post-roll to be written before splicing (spec §6.2 step 2).
            let need_ms = trigger_ms + buffer_cfg.post_ms;
            let deadline = Instant::now() + Duration::from_secs(5);
            while ring.stats().span_ms < need_ms {
                if Instant::now() > deadline {
                    bail!("timed out waiting for post-roll (span={}ms need={}ms)", ring.stats().span_ms, need_ms);
                }
                std::thread::sleep(Duration::from_millis(50));
                ring.scan_once()?;
            }

            let stem = format!("clip-{}", unix_seconds());
            let clip = ring.trigger(trigger_ms, &stem)?;
            tracing::info!(
                "wrote {} ({}ms, {} bytes, encoder={})",
                clip.path.display(),
                clip.duration_ms,
                clip.size_bytes,
                clip.encoder
            );
        }
    }
}

/// Build the encoder. Hardware is the only shipping path.
///
/// `dev_software` exists solely so the pipeline can be smoke-tested on a host with
/// no GPU encoder, and is only reachable when the CLI is built with
/// `--features test-encoders`. It is never reachable from the config file.
fn build_encoder(
    bin: &FfmpegBinaries,
    cfg: &Config,
    scratch_dir: &Path,
    dev_software: bool,
) -> Result<(Box<dyn Encoder>, String)> {
    let (width, height) = parse_output_size(&cfg.encode.output_size, 1920, 1080);
    let codec = match cfg.encode.codec.as_str() {
        "h264" => VideoCodec::H264,
        "hevc" => VideoCodec::Hevc,
        other => bail!("unsupported encode.codec: {other}"),
    };
    let segment_ms = cfg.buffer.segment_time * 1000;

    let encode_cfg = if dev_software {
        #[cfg(feature = "test-encoders")]
        {
            tracing::warn!(
                "--dev-software-encoder: using libx264. This is for smoke-testing the \
                 pipeline only and is NOT a supported configuration."
            );
            EncodeConfig::for_tests_software(
                codec,
                width,
                height,
                cfg.encode.fps,
                scratch_dir.to_path_buf(),
                segment_ms,
            )
        }
        #[cfg(not(feature = "test-encoders"))]
        {
            bail!("--dev-software-encoder requires building with `--features test-encoders`");
        }
    } else {
        let vendor = select_vendor(bin, &cfg.encode.vendor, codec)?;
        EncodeConfig::hardware(
            codec,
            vendor,
            width,
            height,
            cfg.encode.fps,
            cfg.encode.bitrate_kbps,
            segment_ms,
            scratch_dir.to_path_buf(),
        )
    };

    let encoder = FfmpegEncoder::spawn(bin, &encode_cfg)?;
    let name = encoder.active_encoder().to_string();
    Ok((Box::new(encoder), name))
}

/// `"1920x1080"`, or `""` for the supplied default.
fn parse_output_size(spec: &str, default_w: u32, default_h: u32) -> (u32, u32) {
    if spec.trim().is_empty() {
        return (default_w, default_h);
    }
    spec.split_once('x')
        .and_then(|(w, h)| {
            Some((
                w.trim().parse::<u32>().ok()?,
                h.trim().parse::<u32>().ok()?,
            ))
        })
        .unwrap_or((default_w, default_h))
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p localplay-cli config`
Expected: `test result: ok. 3 passed`.

- [ ] **Step 6: Verify the workspace builds**

Run: `cargo check --workspace && cargo test --workspace`
Expected: builds clean; all unit and integration tests pass.

- [ ] **Step 7: Commit**

```bash
git add apps/localplay-cli
git commit -m "feat(cli): config loading and the buffer run loop"
```

---

## Task 14: WGC capture backend (Windows)

**Files:**
- Create: `crates/capture/src/wgc.rs`
- Modify: `crates/capture/src/lib.rs`

This task cannot be compiled or executed on the macOS dev host. Do it on Windows.

- [ ] **Step 1: Add the windows crate with real features**

Run on Windows:

```bash
cargo add windows --target 'cfg(windows)' -p localplay-capture --features \
  Win32_Foundation,Win32_Graphics_Direct3D11,Win32_Graphics_Dxgi,Win32_System_Com,Win32_System_WinRT,Graphics_Capture,Graphics_DirectX_Direct3D11,Foundation
```

- [ ] **Step 2: Implement the backend**

```rust
//! Windows Graphics Capture video backend.
//!
//! WGC hands us a D3D11 texture per frame; we copy it into system memory (BGRA8).
//! That copy is the known cost of the ffmpeg-sidecar encode path (spec §6.1) and is
//! what a Phase 2 Media Foundation backend would remove.

#![cfg(windows)]

use crate::{CaptureBackend, Frame, PixelFormat};
use anyhow::{bail, Context, Result};
use std::time::Duration;

pub struct WgcCapture {
    monitor_index: usize,
    width: u32,
    height: u32,
    started_at: Option<std::time::Instant>,
    frame_index: u64,
}

impl WgcCapture {
    pub fn new(monitor_index: usize) -> Result<Self> {
        let (width, height) = primary_monitor_size()?;
        Ok(Self { monitor_index, width, height, started_at: None, frame_index: 0 })
    }
}

impl CaptureBackend for WgcCapture {
    fn start(&mut self) -> Result<()> {
        self.started_at = Some(std::time::Instant::now());
        self.frame_index = 0;
        // Implementation note: create the D3D11 device, wrap it with
        // `CreateDirect3D11DeviceFromDXGIDevice`, obtain the monitor's
        // `GraphicsCaptureItem`, and start a `Direct3D11CaptureFramePool`.
        // Frames arrive on the pool's `FrameArrived` callback; we stage-copy each
        // texture into a mapped staging buffer and hand the bytes to `Frame`.
        bail!(
            "WGC backend is not yet wired up (monitor {}): {}x{}",
            self.monitor_index,
            self.width,
            self.height
        )
    }

    fn next_frame(&mut self, _timeout: Duration) -> Result<Option<Frame>> {
        let started = self.started_at.context("capture not started")?;
        let _ = started;
        Ok(None)
    }

    fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

fn primary_monitor_size() -> Result<(u32, u32)> {
    // Use GetSystemMetrics(SM_CXSCREEN/SM_CYSCREEN) for the primary monitor.
    bail!("not yet implemented")
}

/// Confirm the pixel format expectations used by `Frame`.
const _: PixelFormat = PixelFormat::Bgra8;
```

> This task is deliberately a scaffold plus a documented implementation note rather
> than fabricated code. The WGC frame-pool wiring is the single most intricate part
> of Phase 1 and must be written on Windows with a debugger attached. Do **not**
> mark this task complete until `localplay-cli buffer` reports a real frame counter.

- [ ] **Step 3: Verify it compiles on Windows**

Run (on Windows): `cargo check -p localplay-capture --target x86_64-pc-windows-msvc`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add crates/capture
git commit -m "feat(capture): WGC backend scaffold for Windows"
```

---

## Task 15: WASAPI loopback audio backend (Windows)

**Files:**
- Create: `crates/capture/src/wasapi.rs`
- Modify: `crates/capture/src/lib.rs`

- [ ] **Step 1: Add the windows audio feature**

```bash
cargo add windows --target 'cfg(windows)' -p localplay-capture --features Win32_Media_Audio,Win32_Media_KernelStreaming
```

- [ ] **Step 2: Implement the backend**

```rust
//! WASAPI loopback audio capture of the default render endpoint (spec §5.1).
//!
//! Loopback is the only zero-config way to capture game audio on Windows: there is
//! no DirectShow device for the default output, so ffmpeg's `dshow` input cannot be
//! used without asking the user to install a virtual audio cable.

#![cfg(windows)]

use crate::{AudioBackend, AudioBuffer, AudioFormat};
use anyhow::{bail, Context, Result};
use std::time::Duration;

/// Resample to a fixed 48kHz stereo so the audio timeline is exactly derivable
/// from the byte count (spec §13).
const TARGET: AudioFormat = AudioFormat { sample_rate: 48_000, channels: 2 };

pub struct WasapiLoopback {
    format: AudioFormat,
    started_at: Option<std::time::Instant>,
    blocks: u64,
}

impl WasapiLoopback {
    pub fn new() -> Result<Self> {
        Ok(Self { format: TARGET, started_at: None, blocks: 0 })
    }
}

impl AudioBackend for WasapiLoopback {
    fn start(&mut self) -> Result<()> {
        self.started_at = Some(std::time::Instant::now());
        self.blocks = 0;
        // Implementation note: CoInitializeEx on the capture thread, activate
        // IAudioClient on the default *render* endpoint, Initialize with
        // AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK, then
        // IAudioCaptureClient::GetBuffer in a loop. Convert the mix format to
        // 48kHz s16le stereo before publishing (the render endpoint's mix format is
        // typically float32 and may not be 48kHz).
        bail!("WASAPI loopback backend is not yet wired up");
    }

    fn next_buffer(&mut self, _timeout: Duration) -> Result<Option<AudioBuffer>> {
        let _ = self.started_at.context("audio capture not started")?;
        Ok(None)
    }

    fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
```

- [ ] **Step 3: Verify it compiles on Windows**

Run (on Windows): `cargo check -p localplay-capture --target x86_64-pc-windows-msvc`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add crates/capture
git commit -m "feat(capture): WASAPI loopback audio backend scaffold"
```

---

## Task 16: `xtask` for sidecars and encoder probing

**Files:**
- Create: `xtask/src/main.rs`, `xtask/sidecars.toml`

- [ ] **Step 1: Add the sidecar manifest**

```toml
# xtask/sidecars.toml
#
# LGPL ffmpeg builds only: localplay never uses libx264/libx265, so GPL builds are
# unnecessary and would impose GPL obligations on the distributed app (README).

[[platform]]
target = "x86_64-pc-windows-msvc"
archive_url = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip"
# Recorded on first fetch via `cargo xtask sidecars record`. Reviewed in the PR.
sha256 = "RECORD_ME"
archive_kind = "zip"
binaries = ["bin/ffmpeg.exe", "bin/ffprobe.exe"]
```

- [ ] **Step 2: Implement the fetch/verify task**

```rust
//! Build helpers: sidecar acquisition and encoder probing.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("sidecars") => match std::env::args().nth(2).as_deref() {
            Some("fetch") => sidecars_fetch(),
            Some("record") => sidecars_record(),
            other => bail!("usage: xtask sidecars <fetch|record>, got {other:?}"),
        },
        Some("probe") => probe_encoders(),
        _ => bail!("usage: xtask <sidecars|probe>"),
    }
}

fn binaries_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../binaries")
}

fn sidecars_fetch() -> Result<()> {
    let manifest = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("sidecars.toml"),
    )
    .context("reading sidecars.toml")?;

    if manifest.contains("RECORD_ME") {
        bail!(
            "sidecars.toml has an unrecorded sha256. Run `cargo xtask sidecars record` on a \
             trusted network, review the diff, and commit the recorded hash."
        );
    }
    // Download to a temp file, verify sha256, then extract into binaries/.
    // Deliberately not implemented with a placeholder: see the note below.
    bail!("not implemented in this task; see the note in the plan")
}

fn sidecars_record() -> Result<()> {
    bail!("not implemented in this task; see the note in the plan")
}

fn probe_encoders() -> Result<()> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .context("running `ffmpeg -encoders`; is ffmpeg on PATH?")?;
    let text = String::from_utf8_lossy(&out.stdout);
    for name in ["h264_nvenc", "hevc_nvenc", "h264_qsv", "hevc_qsv", "h264_amf", "hevc_amf"] {
        let present = text.lines().any(|l| l.contains(name));
        println!("{name:<12} {}", if present { "listed" } else { "absent" });
    }
    println!("\n\"listed\" means ffmpeg advertises it; a 1-frame smoke test is still required.");
    Ok(())
}
```

> `sidecars_fetch`/`sidecars_record` are intentionally unimplemented here. Downloading
> and extracting archives is a self-contained chunk of work with real supply-chain
> consequences, and I am not going to write archive-handling code in a plan and call
> it done. Write it as its own reviewed change: `sidecars_record` downloads, computes
> the sha256 of the archive, and rewrites `sidecars.toml`; `sidecars_fetch` downloads,
> compares against the recorded sha256, refuses on mismatch, and extracts only the
> paths listed in `binaries`. Add the `sha2` and `zip` crates at that point.

- [ ] **Step 3: Verify `probe` runs**

Run: `cargo run -p xtask -- probe`
Expected: prints a line per encoder name with `listed`/`absent`.

- [ ] **Step 4: Commit**

```bash
git add xtask
git commit -m "chore(xtask): encoder probe task and sidecar manifest"
```

---

## Task 17: Windows end-to-end verification runbook

**Files:**
- Create: `docs/runbooks/phase-1-verification.md`
- Modify: `README.md`

- [ ] **Step 1: Write the runbook**

Write `docs/runbooks/phase-1-verification.md` containing, for each of the eight
criteria in the spec (§11), a numbered procedure with the exact command and the
exact expected observation. It must include:

- Criterion 1: `localplay-cli buffer`, then confirm the log reports a monotonically
  increasing frame count and the captured resolution.
- Criterion 2: after a 5-minute soak, confirm the logged `bytes=` never exceeds
  `scratch_cap_bytes`.
- Criterion 3: press `Ctrl+F8`, and record the wall-clock delta between the press and
  the `wrote ...` log line. Expected under 2 s plus `post_seconds`.
- Criterion 4: `ffprobe -v error -show_format -of json <clip>` and compare
  `format.duration` against `pre_seconds + post_seconds` (tolerance ±0.5 s).
- Criterion 5: confirm `ffprobe -show_streams` reports the same video codec the CLI
  logged as its active encoder, and that the clip was written in well under a second.
- Criterion 6: sample the process's CPU and working set during the soak.
- Criterion 7: set `vendor = "nvenc"` on an AMD-only machine and confirm a non-zero
  exit with a message naming the missing encoder.
- Criterion 8: confirm `ffprobe -show_streams` lists exactly one video and one audio
  stream, and confirm lip-sync by playing the clip.

- [ ] **Step 2: Link the runbook from the README**

Add under the Roadmap section:

```markdown
Phase 1 acceptance is verified on Windows hardware using the
[Phase 1 verification runbook](docs/runbooks/phase-1-verification.md).
```

- [ ] **Step 3: Commit**

```bash
git add docs/runbooks README.md
git commit -m "docs: phase 1 verification runbook for the eight acceptance criteria"
```

---

## Self-Review

Run after the plan is complete, before execution.

**1. Spec coverage**

| Spec section | Implementing task |
|---|---|
| §3 stack (WGC, DXGI, WASAPI, ffmpeg sidecar, SQLite, hotkeys) | 2, 8, 11, 12, 14, 15, 16 |
| §5.1 `CaptureBackend` + `AudioBackend` | 8 (traits + stubs), 14, 15 (Windows) |
| §5.2 `Encoder` + vendor selection | 9; vendor probing in 16 |
| §5.3 `RingBuffer`, `Trigger`, `ClipSplicer` | 5, 6, 7, 10, 12 |
| §5.4 `media` driver | 2, 3, 4 |
| §5.5 SQLite schema | 11 |
| §6.1 one ffmpeg child, two pipes | 9 |
| §6.2 trigger flow | 10 (buffer), 13 (CLI wait loop) |
| §6.3 keyframe snapping | 9 (`-force_key_frames`), 4 (tolerance test) |
| §6.4 ledger not media is persisted | 5 (`save_atomic`), 10 (`save_ledger`) |
| §7 loopback-only integrations | 12 (`no_egress` test); LoL/GSI themselves are Phase 4 |
| §8 storage policy + delete ordering | `delete_clip_returning_path` in 11; policy loop is Phase 3 |
| §10 config | 13 |
| §11 criteria 1–8 | 17 (runbook); criteria are executed manually on Windows |
| §13 risks | A/V drift logged in 13; orphan sweep deferred (see gaps) |

**Gaps found and closed:**
- Vendor probing (`ffmpeg -encoders` + smoke test, spec §5.2) had no implementing
  task. Added as `xtask probe` in Task 16; the smoke test itself is wired in Phase 2
  when vendor selection becomes configurable.
- The "orphaned ffmpeg process" risk (spec §13) had no task. It is only reachable once
  the process runs for hours, which Phase 1's soak does not cover. Left as a known
  Phase 2 item and noted here rather than silently dropped.

**2. Placeholder scan**

- Tasks 14, 15 and 16 contain `bail!("not yet implemented")` bodies. These are
  **deliberate**: each is a Windows-only or supply-chain-sensitive chunk that must be
  written with a compiler and a debugger available, and fabricating them would be
  worse than admitting the gap. The runbook in Task 17 cannot pass until they are
  filled in, so the gap cannot be mistaken for completion.
- No `TBD`/`TODO` markers remain elsewhere.

**3. Type consistency**

- `Frame`/`AudioBuffer` defined in Task 8, used identically in Tasks 9, 10, 13.
- `EncodeConfig::hardware` vs `for_tests_software` — Tasks 9, 10 use the latter.
- `RingBuffer::start(bin, cfg, scratch_dir)` — defined in Task 10, called identically
  in Task 13. `cli` imports `localplay_replay::buffer::{BufferConfig, RingBuffer}`,
  matching the module path declared in Task 10's `lib.rs`.
- `SegmentLedger::evict_to_cap` returns `Vec<Segment>` — matches Task 10's eviction loop.
- `Encoder` trait has `submit_video`/`submit_audio`/`finish`/`active_encoder` in Task 9;
  Tasks 10 and 13 call exactly those.
- `Hotkey::parse` returns `Result<Self>` (Task 12) and Task 13 consumes it with `?`.

**4. Known scope boundary**

Task 13 wires `StubCapture` into the CLI. Tasks 14–15 deliver the real Windows
backends but the plan does not include the step that swaps them in behind
`Box<dyn CaptureBackend>`, because that swap is only verifiable on Windows. **The
executor must add that swap on Windows as part of Task 14/15**, gated on
`cfg(windows)`, before the runbook can pass. Flagged rather than hidden.

---

## Execution Handoff

Two execution options:

**1. Subagent-Driven (recommended)** — a fresh subagent per task, reviewed between
tasks, fast iteration. Tasks 1–13 and 16 run anywhere; Tasks 14, 15, 17 need Windows.

**2. Inline Execution** — work through the tasks in one session with checkpoints, using
the superpowers-executing-plans skill.
