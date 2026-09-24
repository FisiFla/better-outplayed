# Phase 5 — session recording, the microphone and the game watcher

How to drive the new surface, and what has actually been observed. Written the way
`phase-1-verification.md` is: every claim is tagged with how it is known — *run here*,
*read from the code*, or *not verified*.

Everything below was run on the development host (macOS, ffmpeg on `PATH`, no GPU encoder)
with the synthetic sources, which is the only place this pipeline can be exercised without
Windows. **No Windows run exists** for any of it.

---

## 1. The configuration surface

```toml
[recorder]
mode = "buffer"          # or "session" — the CLI overrides it with --mode

[storage.sessions]       # additive: a pre-Phase-5 [storage] table keeps working
sessions_dir = ""        # empty = <app data dir>/sessions
max_total_bytes = 214748364800
max_age_days = 7

[mic]
enabled = false          # off by default; the opt-in is this line

[games]
auto_record = false      # off by default: nothing is watched, no thread exists
poll_ms = 2500
# [[games.watch]] name/exe/signal entries replace the default list
```

`[recorder] mode`, `[mic]` and `[games]` travel into the engine in
`localplay_recorder::RecorderOptions`, not as fields on `RecorderConfig`: the desktop shell
is a separate workspace that builds a `RecorderConfig` field by field, so a new required
field there would break a crate this change does not own. `Recorder::start` is
`start_with_options(cfg, RecorderOptions::default())` — buffer mode, no microphone, nothing
watched — so every existing caller keeps the behaviour it had.

## 2. The CLI recipe (run here, both modes, microphone off and on)

```sh
export CARGO_HOME="$PWD/target/cargo-home"
run() {  # $1 = app dir name, $2 = config file, rest = extra flags
  dir="target/phase5-e2e/$1"; rm -rf "$dir"; mkdir -p "$dir/localplay"
  cp "$2" "$dir/localplay/config.toml"
  LOCALAPPDATA="$PWD/$dir" RUST_LOG=info cargo run -q -p localplay-cli --features test-encoders -- \
    buffer --dev-software-encoder --dev-stub-sources --self-test-clip-after 4 "$@"
}
run buffer  target/phase5-e2e/config-buffer.toml
run session target/phase5-e2e/config-session.toml --mode session
run session-mic target/phase5-e2e/config-session-mic.toml --mode session   # [mic] enabled = true
```

`--dev-stub-sources` is new and feature-gated exactly like `--dev-software-encoder`: it is
the only way the microphone path can run off Windows, because there is no non-Windows
microphone backend and a stub selected implicitly would be worse than an error. It prints
`NOTHING REAL IS CAPTURED` before it starts.

Observed (excerpts of real runs):

```
INFO localplay_recorder: session #1 opened (session): segments in .../sessions/session-1790211072 (the scratch cap does not apply)
INFO localplay_recorder: hotkey pressed: media=4000ms wall=7030ms (drift 3030ms); waiting for post-roll
INFO localplay_recorder::index: indexed clip #1 (media t=2000ms, 3187ms, 1023170 bytes, libx264): .../clips/clip-1790211080.mp4
INFO localplay_recorder: session #1 finalised: .../sessions/session-1790211072.mp4 (8051ms, 2122859 bytes) concatenated from 9 segment(s); 2132516 bytes of temporary segments removed
```

and, with `[mic] enabled = true`:

```
INFO localplay_recorder: capture geometry 1280x720 at 10fps (frame counter starts at 0), plus a microphone track on port Some(59590)
INFO localplay_recorder::session: session .../clips/clip-1790211174.mp4 carries 2 audio tracks (game audio + microphone)
INFO localplay_recorder::session: session .../sessions/session-1790211166.mp4 carries 2 audio tracks (game audio + microphone)
```

`ffprobe` on those files: `video audio audio` for both the clip and the session file
(1 video + 2 audio). The cleanup pass, with 1 KB caps, on the next start:

```
INFO localplay_recorder::index: storage policy: deleted 1 clip(s) and 1 session(s), reclaimed 3330354 bytes;
     the clips directory now holds 0 bytes against its 1000 byte cap, and the session store 0 bytes against its 1000 byte cap
```

## 3. What a full session does, and where it can fail

* Segments go into `sessions/session-<start>/`; `buffer.scratch_cap_bytes` is **not**
  applied to them (structurally: the session's ledger never calls `evict_to_cap`).
* At stop the segments are concatenated with `-c copy` into `sessions/session-<start>.mp4`
  and the temporary segments are removed. The finalise builds its own concat rather than
  calling `ClipSplicer::splice`, for two measured reasons: the splicer's ffmpeg invocation
  has **no `-map`**, so the concat demuxer's default selection keeps one audio stream and a
  microphone track would be dropped; and `edit::concat_lossless` bounds the copy with a fixed
  60 s timeout, which a multi-GB session on a slow volume exceeds — and would then fail again
  on every retry. The list format, the Windows `\\?\` path normalisation
  (`concat_list_path`), `-c copy -movflags +faststart` and the ffprobe check are reused.
* **Not enough space**: the concatenation needs transiently about twice the session's bytes.
  There is no pre-flight check; ffmpeg fails, the partial output is removed, **every segment
  is kept**, the `sessions` row is left running (`ended_at IS NULL` — a running session is
  never evicted, so retention cannot delete footage that has no session file), and the error
  is returned by `stop()`. The next start's recovery pass finishes the job once there is
  room. *Run here* for the failure path (`cargo test -p localplay-recorder` — a session whose
  segments are unreadable); *not verified* against a real full disk.
* **A crash mid-session** leaves a running row and a directory of segments. The next start
  concatenates them into the file the session would have written, closes the row with the
  real size, and removes the segments; a session that cannot be finalised is reported and
  retried at every start; a directory no row names is reported and never deleted.

## 4. Honest limitations

1. **Clips taken with the microphone on, in buffer mode, carry only the game audio.** The
   ring's trigger splices clips with `ClipSplicer::splice`, whose concat has no `-map`
   (`localplay-replay` is not editable here), so the second audio track is dropped. The
   engine says so in a warning at start. In session mode the clip keeps both tracks, because
   that trigger is this crate's. Fixing buffer mode properly means `-map 0` in
   `localplay_media::edit::concat_lossless`, which is outside this crate.
2. **Buffer-mode session rows share one scratch directory.** Their `size_bytes` is "the bytes
   the ring holds", so two buffer sessions can count the same segments. Full-session rows are
   exact (their own directory).
3. **The session timeline (`Store::events_for_session`) is not populated by this engine.**
   `sessions.started_at` must be the wall clock (the retention rules age by it), while
   `events.at` is the media clock this engine has always written, and `offset_ms` is derived
   as their difference. `events.session_id` therefore stays NULL, exactly as before Phase 5;
   wiring the two would need an explicit per-session media origin in the schema.
4. **The 4K sustained-rate issue is unchanged** (a separate open issue). A full session
   records at whatever rate the machine sustains; the media timeline is the frames' arrival
   timestamps, so a `pre_seconds` window is still that many real seconds, but at 4K the
   picture can hold frames that were dropped. It does not make full-session recording
   *unsafe* — it makes the file's smoothness a property of the machine, and the status line's
   `dropped=` is where that shows.
5. **Windows-only paths are unrun.** `microphone_backend()` (WASAPI communications endpoint)
   has never executed; the game watcher's process enumeration has never executed. The game
   watcher's *driving* half (start/stop recording on presence changes) is run here.
6. **`localplay-recorder` and `localplay-cli` are not cross-checkable for
   `x86_64-pc-windows-msvc`** — they depend on `localplay-store` → bundled rusqlite C, which
   cannot be built for MSVC from macOS. The recorder's new code has no `#[cfg(windows)]` of
   its own (the platform rule lives in `localplay-capture`), so what this costs is small.
