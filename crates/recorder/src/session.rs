//! Full-session recording: the per-session segment directory, its ledger, the lossless
//! concatenation at stop, and the recovery of a session a crash left behind (Phase 5).
//!
//! # Why this is not the replay ring
//!
//! The two modes look alike — a directory of `seg-%06d.mp4` files written by one ffmpeg
//! segment muxer — and they are deliberately *not* the same object, because the one thing
//! that defines the ring is the one thing a full session must not have: eviction.
//! [`localplay_replay::buffer::RingBuffer::scan_once`] indexes new segments **and** calls
//! `SegmentLedger::evict_to_cap`, which deletes the oldest segment files until the ring is
//! under `buffer.scratch_cap_bytes`. A four-hour session has no oldest segment to spare,
//! so [`SessionRing`] scans the same directory names with the same
//! [`localplay_replay::scanner`] functions and never evicts anything.
//!
//! That bypass is *structural*, not a huge cap: `evict_to_cap` is not called on this
//! ledger at all, and `localplay-replay` is a dependency this crate may not change (its
//! `RingBuffer` has no "scan without evicting" entry point — see `finalise` for the one
//! other place that limit shows, and the report that goes with this work for the gap).
//! The re-expression is deliberately small: the scan, the sequence reservation and the
//! clip window are [`localplay_replay`]'s public pieces
//! ([`SegmentLedger`], [`scanner`], [`window`], [`ClipSplicer`]); nothing here re-implements
//! a concatenation.
//!
//! # The on-disk shape
//!
//! ```text
//! <app data dir>/sessions/
//! ├── session-1758612345/            <- the per-session segment directory
//! │   ├── seg-000000.mp4             <- written by the encoder's segment muxer
//! │   ├── seg-000001.mp4
//! │   └── ...
//! └── session-1758612345.mp4         <- written at stop, `-c copy`, segments then removed
//! ```
//!
//! There is no ledger file: a session's segments are enumerated from the directory itself
//! (they are named `seg-%06d.mp4` in sequence order), which is also what makes recovery
//! possible after a crash — nothing has to have been persisted for the footage to be
//! findable, and nothing is orphaned by a write that never happened.

use crate::index::now_ms;
use anyhow::{bail, Context, Result};
use localplay_media::{edit, FfmpegBinaries, MediaInfo};
use localplay_replay::ledger::{Segment, SegmentLedger};
use localplay_replay::scanner::{self, newly_complete, next_segment_number};
use localplay_replay::window;
use localplay_replay::splice::{ClipMetadata, ClipSplicer};
use localplay_store::{Session, Store, SESSION_MODE_SESSION};
use std::path::{Path, PathBuf};
use std::process::Command;

/// How many whole-session concatenations were attempted before giving up on a name.
const MAX_SESSION_DIR_ATTEMPTS: u32 = 100;

/// A session's segments, indexed and **never evicted**.
///
/// The [`localplay_replay::buffer::RingBuffer`]'s cap-free twin: same directory convention,
/// same "a segment is only trusted once a strictly later one exists" scan rule, same
/// timeline arithmetic — and no eviction, by construction (there is no cap here to pass).
pub struct SessionRing {
    bin: FfmpegBinaries,
    dir: PathBuf,
    clips_dir: PathBuf,
    ledger: SegmentLedger,
    highest_known: Option<u64>,
    segment_ms: u64,
    /// Offset of this run's first segment on the ledger timeline — `reserve_number() *
    /// segment_ms`, exactly as the ring's is, so the clip path's arithmetic is the same in
    /// both modes.
    origin_ms: u64,
    /// Name of the encoder in use, recorded on every clip (spec §11 criterion 5).
    encoder: String,
    /// Media time kept before a trigger (the engine's `buffer.pre_seconds`).
    pre_ms: u64,
    /// Media time kept after a trigger (the engine's `buffer.post_seconds`).
    post_ms: u64,
    /// Whether the encoder that writes this session's segments has a microphone input. It
    /// decides how a **clip** is spliced out of the session: with a second audio track the
    /// splicer's `-c copy` concat would drop it, so the map-preserving concat is used
    /// instead (`finalise` has the measurement; the same `-map 0` argument, one window).
    microphone: bool,
}

impl SessionRing {
    /// Open the session's segment directory, creating it if the encoder is about to write
    /// into it. Nothing is scanned yet — [`SessionRing::adopt_existing`] is that step, and
    /// the caller runs it before the encoder is spawned (the segment number ffmpeg is told
    /// depends on it).
    ///
    /// The `BufferConfig` carries the segment length and the trigger window, exactly as it
    /// does for the replay ring — the two modes resolve a clip the same way, and taking the
    /// same configuration type is what keeps that true. Its `scratch_cap_bytes` is the one
    /// field this ring does not read: it is the *ring's* rule (see the module docs).
    pub fn open(
        bin: &FfmpegBinaries,
        dir: &Path,
        cfg: &localplay_replay::buffer::BufferConfig,
        microphone: bool,
        encoder: String,
    ) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating the session directory {}", dir.display()))?;
        std::fs::create_dir_all(&cfg.clips_dir)
            .with_context(|| format!("creating {}", cfg.clips_dir.display()))?;
        Ok(Self {
            bin: bin.clone(),
            dir: dir.to_path_buf(),
            clips_dir: cfg.clips_dir.clone(),
            ledger: SegmentLedger::default(),
            highest_known: None,
            segment_ms: cfg.segment_ms,
            pre_ms: cfg.pre_ms,
            post_ms: cfg.post_ms,
            microphone,
            origin_ms: 0,
            encoder,
        })
    }

    /// Index what is already in the directory (a session directory is fresh, so this is
    /// normally nothing — but a caller that restarts into an adopted directory gets the
    /// ring's own answer rather than an overwrite).
    pub fn adopt_existing(&mut self) -> Result<usize> {
        let before = self.ledger.len();
        self.scan()?;
        Ok(self.ledger.len() - before)
    }

    /// Index every segment in the directory, including the newest file.
    ///
    /// [`SessionRing::scan`] deliberately does not trust the newest segment — ffmpeg
    /// appends to it, so it may be half-written. That rule is right while a recording is
    /// running and wrong at shutdown, when the encoder has been flushed and the child has
    /// exited: at that point the newest file is complete, and dropping it would lose the
    /// last `segment_time` of every session.
    pub fn scan_all(&mut self) -> Result<()> {
        for segment in segments_on_disk(&self.dir, self.segment_ms)? {
            let seq = segment.seq;
            self.ledger.push(segment);
            self.highest_known = Some(self.highest_known.map_or(seq, |known| known.max(seq)));
        }
        Ok(())
    }

    /// Index newly completed segments. **No eviction happens here** — that is the whole
    /// difference from `RingBuffer::scan_once` (see the module docs).
    pub fn scan(&mut self) -> Result<()> {
        let observed = observed_seqs(&self.dir)?;
        for seq in newly_complete(&observed, self.highest_known) {
            let file = self.dir.join(format!("seg-{seq:06}.mp4"));
            let bytes = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            self.ledger.push(Segment {
                seq,
                file,
                start_ms: seq * self.segment_ms,
                duration_ms: self.segment_ms,
                bytes,
            });
            self.highest_known = Some(seq);
        }
        Ok(())
    }

    /// Reserve the sequence number the encoder must start writing at — the same rule the
    /// ring uses ([`next_segment_number`]), so a session directory that already holds
    /// segments cannot be overwritten.
    pub fn reserve_number(&mut self) -> Result<u64> {
        let names = observed_names(&self.dir)?;
        let number = next_segment_number(&self.ledger.seqs(), &names);
        self.origin_ms = number.saturating_mul(self.segment_ms);
        Ok(number)
    }

    /// The ledger's position, in ms, of this run's zero (see the field's doc).
    pub fn origin_ms(&self) -> u64 {
        self.origin_ms
    }

    /// This run's recorded media time, in ms — the same quantity
    /// `RingBuffer::stats().span_ms` is, measured from the same zero.
    pub fn span_ms(&self) -> u64 {
        self.ledger.span_ms().saturating_sub(self.origin_ms)
    }

    /// Complete segments, or the ledger as it stands.
    pub fn segments(&self) -> &[Segment] {
        self.ledger.segments()
    }

    /// Bytes the session currently occupies on disk.
    pub fn bytes_on_disk(&self) -> u64 {
        self.ledger.total_bytes()
    }

    pub fn segment_count(&self) -> usize {
        self.ledger.len()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The ffmpeg binaries this session's splices run with (the caller's own, cloned at
    /// [`SessionRing::open`]).
    pub fn bin(&self) -> &FfmpegBinaries {
        &self.bin
    }

    /// Extract a clip around `trigger_ms` from the session's own footage.
    ///
    /// The ring's trigger, restated (see the module docs on why): resolve the window on
    /// the ledger, splice via [`ClipSplicer::splice`] and warn about a truncated pre-roll
    /// exactly as the ring does. Taking a clip during a full session is the point of having
    /// both modes, and it is the *same* splice the ring mode uses — only the window it is
    /// given comes from this ledger.
    pub fn trigger(&self, trigger_ms: u64, stem: &str) -> Result<ClipMetadata> {
        let win = window::resolve(
            &self.ledger,
            // `trigger_ms` is run-relative; the ledger's timeline is absolute — the same
            // conversion the ring's trigger makes.
            self.origin_ms + trigger_ms,
            self.pre_ms,
            self.post_ms,
        )
        .map_err(|err| anyhow::anyhow!("{err}"))?;
        if win.truncated_front {
            tracing::warn!(
                "only {}ms of pre-roll was buffered for a {}ms request",
                win.segments.first().map(|_| win.duration_ms()).unwrap_or(0),
                self.pre_ms
            );
        }
        let out = self.clips_dir.join(format!("{stem}.mp4"));
        if self.microphone {
            // A clip cut out of a session that carries a microphone track must keep it: the
            // session file does (see `finalise`), and a clip that silently lost the voice
            // track would be the one artefact of a recording that is missing it.
            finalise(&self.bin, &out, &win.segments, &self.encoder, true)
        } else {
            ClipSplicer::splice(&self.bin, &win, &out, &self.encoder)
        }
    }
}

/// The pump's view of a session's segments: the same two facts the ring answers, with no
/// eviction behind the scan (see the module docs).
impl crate::pump::MediaRing for SessionRing {
    fn scan(&mut self) -> Result<()> {
        SessionRing::scan(self)
    }

    fn span_ms(&self) -> u64 {
        SessionRing::span_ms(self)
    }
}

/// The microphone's own safety net (see `finalise`).
fn observed_names(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

/// The sequence numbers of the segments in `dir` (everything else in the directory — a
/// concat list, a stray file — is not a segment).
fn observed_seqs(dir: &Path) -> Result<Vec<u64>> {
    Ok(observed_names(dir)?.iter().filter_map(|n| scanner::segment_seq(n)).collect())
}

/// Every segment file in `dir`, in sequence order, with the sizes they actually have.
///
/// The ledger is built from the *directory*, not from a saved index: a crash leaves
/// segments on disk and nothing else, so the footage has to be findable from the file
/// names alone. A zero-byte file is skipped — ffmpeg creates the next segment's file
/// before writing into it, and an empty file is not a second of footage.
pub fn segments_on_disk(dir: &Path, segment_ms: u64) -> Result<Vec<Segment>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut segments: Vec<Segment> = observed_seqs(dir)?
        .into_iter()
        .map(|seq| {
            let file = dir.join(format!("seg-{seq:06}.mp4"));
            Segment {
                seq,
                file,
                start_ms: seq * segment_ms,
                duration_ms: segment_ms,
                bytes: std::fs::metadata(dir.join(format!("seg-{seq:06}.mp4")))
                    .map(|m| m.len())
                    .unwrap_or(0),
            }
        })
        .filter(|segment| segment.bytes > 0)
        .collect();
    segments.sort_by_key(|s| s.seq);
    Ok(segments)
}

/// A directory name for a new session: `session-<unix seconds>`, or with `-2`, `-3`, … when
/// a run already started in that second.
///
/// The name is *attempted by creation* (`create_dir`, which fails rather than overwriting),
/// so two processes starting in the same second cannot share a segment directory — the
/// one failure that would mix two recordings' segments into one session file.
pub fn create_session_dir(sessions_dir: &Path, started_at_ms: i64) -> Result<PathBuf> {
    std::fs::create_dir_all(sessions_dir)
        .with_context(|| format!("creating {}", sessions_dir.display()))?;
    // The same instant the session row's `started_at` carries and `session_file_path` names
    // the file from, so a session's directory and its file read as one session in a file
    // browser rather than as two that merely started close together.
    let stamp = started_at_ms.div_euclid(1_000) as u64;
    for attempt in 1..=MAX_SESSION_DIR_ATTEMPTS {
        let name = if attempt == 1 {
            format!("session-{stamp}")
        } else {
            format!("session-{stamp}-{attempt}")
        };
        let dir = sessions_dir.join(name);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("creating {}", dir.display()));
            }
        }
    }
    bail!(
        "could not find a free session directory under {} after {MAX_SESSION_DIR_ATTEMPTS} \
         attempts in the same second; is something else creating them?",
        sessions_dir.display()
    )
}

/// The concatenated file a session with this start instant writes:
/// `sessions/session-<unix seconds>.mp4`.
///
/// Derived from the session's own start instant — not from "now" — so a retry (recovery of
/// a session a crash left unfinalised) writes the same path it would have written the first
/// time, rather than a second file beside a partial one.
pub fn session_file_path(sessions_dir: &Path, started_at_ms: i64) -> PathBuf {
    let seconds = started_at_ms.div_euclid(1_000);
    sessions_dir.join(format!("session-{seconds}.mp4"))
}

/// Concatenate a finished session's segments into one file.
///
/// `segments` must be in sequence order and complete; the caller flushes the encoder first
/// and takes the list from [`segments_on_disk`] (or [`SessionRing::scan_all`]).
///
/// # Sharing the lossless path with the splicer
///
/// This uses the same three steps `ClipSplicer::splice` does — `check_concat_layout`,
/// `write_concat_list`, `concat_lossless_sized` — rather than keeping a private copy. It
/// used to have its own concat, for two measured reasons that no longer exist: the splicer
/// passed no `-map`, so ffmpeg's default stream selection kept one audio stream and dropped
/// the microphone track from every microphone-enabled *clip*; and it bounded the copy with a
/// clip-sized 60s, which is the wrong budget for a 20GB session and fails only after doing
/// the work, on every recovery attempt. Both now live in `localplay_media::edit`, where one
/// implementation serves both callers; a private copy here would be a second place for them
/// to drift apart.
///
/// What this adds over `splice` is what a *session* needs and a clip does not: a partial
/// output is deleted rather than left occupying the space its own retry needs, and a session
/// that recorded a microphone has its audio-stream count verified afterwards.
pub fn finalise(
    bin: &FfmpegBinaries,
    out: &Path,
    segments: &[Segment],
    encoder: &str,
    multitrack: bool,
) -> Result<ClipMetadata> {
    if segments.is_empty() {
        bail!("no segments to concatenate: this session recorded nothing");
    }
    let files: Vec<PathBuf> = segments.iter().map(|s| s.file.clone()).collect();

    // Refused before anything is written: a segment whose layout differs from the first
    // would make `-map 0` silently drop or truncate a track.
    edit::check_concat_layout(bin, &files)?;

    let list = out.with_extension("concat.txt");
    let total_bytes = edit::write_concat_list(&files, &list)?;
    // The list is scratch whether the copy works out or not.
    let concat = edit::concat_lossless_sized(bin, &list, out, total_bytes);
    let _ = std::fs::remove_file(&list);
    if let Err(e) = concat {
        // The partial file is worse than useless: it occupies the space the retry needs and
        // it is not a session. Removing it is the only thing that gets that space back.
        let _ = std::fs::remove_file(out);
        return Err(e).with_context(|| {
            format!(
                "concatenating {} segment(s) into {}",
                segments.len(),
                out.display()
            )
        });
    }

    let info = MediaInfo::probe(bin, out)
        .with_context(|| format!("probing the concatenated session {}", out.display()))?;
    if info.video.is_none() {
        tracing::warn!(
            "{} has no video stream: the concatenation produced a file ffprobe cannot see \
             a picture in",
            out.display()
        );
    }
    // The microphone's own safety net. `-map 0` is what keeps the second audio track, and a
    // session file that lost it looks exactly like one that never had it — the track is
    // simply absent. Counting the audio streams is the only way to say so out loud.
    if multitrack {
        match audio_stream_count(bin, out) {
            Some(count) if count < 2 => tracing::warn!(
                "{} has {count} audio stream(s) but this session recorded a microphone: the \
                 microphone track is missing from the session file (the segments under the \
                 session directory still have both tracks)",
                out.display()
            ),
            Some(count) => tracing::info!(
                "session {} carries {count} audio tracks (game audio + microphone)",
                out.display()
            ),
            None => tracing::warn!(
                "could not count the audio streams of {}: whether the microphone track \
                 survived the concatenation is unverified for this session",
                out.display()
            ),
        }
    }

    Ok(ClipMetadata {
        path: out.to_path_buf(),
        duration_ms: info.duration_ms,
        size_bytes: info.size_bytes,
        encoder: encoder.to_string(),
    })
}

/// How many audio streams `path` carries, or `None` when ffprobe cannot say.
///
/// A deliberate second probe: [`MediaInfo`] reports the *first* video and the *first* audio
/// stream, which is exactly the shape that cannot distinguish "the microphone survived" from
/// "it did not". Used only for a session that recorded a microphone.
fn audio_stream_count(bin: &FfmpegBinaries, path: &Path) -> Option<usize> {
    let output = Command::new(&bin.ffprobe)
        .args(["-v", "error", "-select_streams", "a", "-show_entries", "stream=index", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
    )
}

/// What a recovery pass found and did, for the caller to log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Recovery {
    /// Sessions that were concatenated and closed by this pass.
    pub recovered: Vec<i64>,
    /// Sessions whose segments could not be concatenated (the row is left running, so the
    /// next start tries again).
    pub unfinalised: Vec<i64>,
    /// Rows for a recording that left nothing on disk, closed with no file.
    pub closed_empty: Vec<i64>,
    /// Directories under the sessions area that no session row names.
    pub orphan_dirs: Vec<PathBuf>,
    /// Session directories that still hold segments although their session is finished (a
    /// crash between the concatenation and the directory's removal). Left in place: the
    /// retention pass removes them with the row that names them.
    pub leftover_dirs: Vec<PathBuf>,
}

impl Recovery {
    /// Whether this pass changed anything worth a log line.
    pub fn is_empty(&self) -> bool {
        self.recovered.is_empty()
            && self.unfinalised.is_empty()
            && self.closed_empty.is_empty()
            && self.orphan_dirs.is_empty()
            && self.leftover_dirs.is_empty()
    }
}

/// Find what a previous run left behind, finish it, and say what was found (Phase 5).
///
/// A crash — a power cut, a killed process, a panic in a driver — leaves three things: a
/// `sessions` row with `ended_at IS NULL`, a directory of segments under the sessions area,
/// and no session file. This pass runs at every start, before this run creates anything, and
/// it is the answer to "does the next start orphan gigabytes?" — it does not:
///
/// * a session row with segments on disk is **concatenated** into the file it would have
///   written (`session_file_path`, from its own start instant) and closed with the real
///   size; the segments are then removed. If the concatenation fails — a full disk is the
///   interesting case — the row is **left running** and the segments are kept, so the next
///   start tries again once there is room (a running session is never an eviction
///   candidate, so retention cannot delete the only copy of the footage meanwhile);
/// * a row whose session wrote no segments at all (a crash between the row and the first
///   segment, or a start that failed after opening the row) is closed with no file;
/// * a buffer-mode row left running is closed with the size it had — its scratch directory
///   is shared with the ring, which is the ring's to manage, so nothing on disk is touched;
/// * a directory no row names is **reported**, never deleted: without a row there is no
///   proof the footage is this application's to remove, and a report is reversible.
///
/// `encoder` is a label only (see [`ClipMetadata::encoder`]): the encoder that wrote the
/// segments lived in a previous process and is not knowable from the files.
pub fn recover_sessions(
    store: &Store,
    sessions_dir: &Path,
    bin: &FfmpegBinaries,
    segment_ms: u64,
    encoder: &str,
) -> Recovery {
    let mut report = Recovery::default();
    let rows = match store.list_sessions() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!("could not read the session store to recover unfinished sessions: {err:#}");
            return report;
        }
    };
    let named: Vec<PathBuf> = rows.iter().map(|row| PathBuf::from(&row.scratch_dir)).collect();

    for row in &rows {
        if row.mode == SESSION_MODE_SESSION {
            recover_session_row(store, row, sessions_dir, bin, segment_ms, encoder, &mut report);
        } else if row.ended_at_ms.is_none() {
            // A buffer-mode session (or a mode a newer build wrote): its footage belongs to
            // the ring, which manages the scratch directory by itself. All this pass owes it
            // is an end stamp, so the retention rules can see it as history.
            match store.end_session(row.id, now_ms(), None, row.size_bytes) {
                Ok(()) => {
                    tracing::info!(
                        "closed the {} session #{} a previous run left running ({} bytes)",
                        row.mode,
                        row.id,
                        row.size_bytes
                    );
                    report.closed_empty.push(row.id);
                }
                Err(err) => tracing::warn!(
                    "could not close the {} session #{} a previous run left running: {err:#}",
                    row.mode,
                    row.id
                ),
            }
        }
    }

    // Directories under the sessions area that no row names. Reported, never removed.
    if let Ok(entries) = std::fs::read_dir(sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || named.contains(&path) {
                continue;
            }
            match segments_on_disk(&path, segment_ms) {
                Ok(segments) if !segments.is_empty() => {
                    tracing::warn!(
                        "{} holds {} session segment(s) ({} bytes) that no session row names: \
                         nothing will delete them, and they are not in the library. Inspect it \
                         and remove it by hand, or leave it — this report repeats at every start.",
                        path.display(),
                        segments.len(),
                        segments.iter().map(|s| s.bytes).sum::<u64>()
                    );
                    report.orphan_dirs.push(path);
                }
                _ => {}
            }
        }
    }
    report
}

/// One session row's recovery (see [`recover_sessions`]).
fn recover_session_row(
    store: &Store,
    row: &Session,
    sessions_dir: &Path,
    bin: &FfmpegBinaries,
    segment_ms: u64,
    encoder: &str,
    report: &mut Recovery,
) {
    let dir = PathBuf::from(&row.scratch_dir);
    let segments = match segments_on_disk(&dir, segment_ms) {
        Ok(segments) => segments,
        Err(err) => {
            tracing::warn!(
                "could not read the session directory {} of session #{}: {err:#}",
                dir.display(),
                row.id
            );
            return;
        }
    };
    let running = row.ended_at_ms.is_none();

    if segments.is_empty() {
        if running {
            match store.end_session(row.id, now_ms(), None, row.size_bytes) {
                Ok(()) => {
                    tracing::info!(
                        "session #{} was left running by a previous run and recorded nothing; \
                         closed with no file",
                        row.id
                    );
                    report.closed_empty.push(row.id);
                }
                Err(err) => tracing::warn!("could not close session #{}: {err:#}", row.id),
            }
            // An empty directory is this application's own and costs nothing to remove.
            if dir.is_dir() {
                if let Err(err) = std::fs::remove_dir(&dir) {
                    tracing::debug!("could not remove the empty {}: {err}", dir.display());
                }
            }
        }
        return;
    }

    if !running && row.final_path.is_some() {
        // A finished session whose segments are still there: a crash between the
        // concatenation and the directory's removal. The final file exists and the row
        // names it, so the segments are a duplicate — reported, not deleted (the retention
        // pass removes them, with the row, when it evicts the session).
        tracing::warn!(
            "session #{} is finished ({}) but its segment directory {} still holds {} \
             segment(s); the retention pass removes them with the session",
            row.id,
            row.final_path.as_deref().unwrap_or("no file"),
            dir.display(),
            segments.len()
        );
        report.leftover_dirs.push(dir);
        return;
    }

    let out = session_file_path(sessions_dir, row.started_at_ms);
    let bytes: u64 = segments.iter().map(|s| s.bytes).sum();
    // `true` asks for the audio-stream count (and its warning): a recovered session may well
    // have had a microphone — the row does not say, and the encoder that knew is gone — and a
    // silently absent voice track is exactly what that check exists to catch.
    match finalise(bin, &out, &segments, encoder, true) {
        Ok(meta) => {
            let path = meta.path.display().to_string();
            match store.end_session(row.id, now_ms(), Some(&path), meta.size_bytes as i64) {
                Ok(()) => {
                    tracing::info!(
                        "recovered session #{} left by a previous run: {} segment(s) ({bytes} \
                         bytes) concatenated into {} ({} bytes)",
                        row.id,
                        segments.len(),
                        path,
                        meta.size_bytes
                    );
                    report.recovered.push(row.id);
                }
                Err(err) => {
                    tracing::warn!(
                        "recovered session #{} into {} but could not close its row: {err:#}",
                        row.id,
                        path
                    );
                    report.unfinalised.push(row.id);
                    return;
                }
            }
            remove_session_dir(&dir);
        }
        Err(err) => {
            tracing::error!(
                "could not recover session #{} left by a previous run: {err:#}. Its {} \
                 segment(s) ({bytes} bytes) are kept in {}, the row is left running, and the \
                 next start tries again — a running session is never evicted, so nothing \
                 will delete this footage meanwhile. Free space if that was the reason.",
                row.id,
                segments.len(),
                dir.display()
            );
            report.unfinalised.push(row.id);
        }
    }
}

/// Remove a session's segment directory, reporting what could not go.
pub fn remove_session_dir(dir: &Path) -> u64 {
    let bytes: u64 = segments_on_disk(dir, 0)
        .map(|segments| segments.iter().map(|s| s.bytes).sum())
        .unwrap_or(0);
    match std::fs::remove_dir_all(dir) {
        Ok(()) => bytes,
        Err(err) => {
            tracing::warn!(
                "could not remove the session directory {}: {err} — its segments stay on \
                 disk; a later pass or the retention rule (through the session row) removes \
                 them",
                dir.display()
            );
            0
        }
    }
}

/// Write two one-second segments into `dir` with a real ffmpeg — 1 video plus `audio` audio
/// streams, the shape the encoder's segment muxer produces.
///
/// **Test-only**, and shared on purpose: it is what lets a test build a directory that looks
/// exactly like a session that was recorded — for the session module's own unit tests and
/// for the engine's crash-recovery test, which needs a session on disk that no live
/// recording can leave behind (a clean stop removes it).
#[cfg(test)]
pub(crate) fn layout_test_segments(dir: &Path, audio: usize) -> Result<Vec<Segment>> {
    let bin = FfmpegBinaries::discover(None)
        .map_err(|err| anyhow::anyhow!("ffmpeg on PATH is needed for the test fixtures: {err}"))?;
    std::fs::create_dir_all(dir)?;
    for seq in 0..2u64 {
        let out = dir.join(format!("seg-{seq:06}.mp4"));
        let mut cmd = Command::new(&bin.ffmpeg);
        cmd.args([
            "-hide_banner", "-loglevel", "error", "-y",
            "-f", "lavfi", "-i", "testsrc=size=64x48:rate=10:duration=1",
            "-f", "lavfi", "-i", "sine=frequency=440:duration=1",
        ]);
        if audio == 2 {
            cmd.args(["-f", "lavfi", "-i", "sine=frequency=880:duration=1"]);
        }
        cmd.args(["-c:v", "libx264", "-g", "10", "-c:a", "aac", "-map", "0:v", "-map", "1:a"]);
        if audio == 2 {
            cmd.args(["-map", "2:a"]);
        }
        cmd.args(["-t", "1"]).arg(&out);
        let output = cmd.output().context("running ffmpeg to build a segment")?;
        if !output.status.success() {
            bail!(
                "building {} failed: {}",
                out.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    segments_on_disk(dir, 1_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use localplay_store::SESSION_MODE_BUFFER;

    /// The ffmpeg on `PATH`, discovered the way the engine discovers it.
    fn bin() -> FfmpegBinaries {
        FfmpegBinaries::discover(None).expect("ffmpeg on PATH")
    }

    /// The ring configuration the session's tests open with: the same shape the engine
    /// passes (`BufferConfig`), with the segment length and window these tests use.
    fn session_cfg(clips_dir: &Path) -> localplay_replay::buffer::BufferConfig {
        localplay_replay::buffer::BufferConfig {
            pre_ms: 2_000,
            post_ms: 1_000,
            scratch_cap_bytes: 1 << 30,
            segment_ms: 1_000,
            clips_dir: clips_dir.to_path_buf(),
        }
    }

    /// [`layout_test_segments`], with the temp-dir conventions of this module's tests.
    fn write_segments(dir: &Path, audio: usize) -> Vec<Segment> {
        layout_test_segments(dir, audio).expect("laying out test segments")
    }

    /// A temp directory **under `target/`**, so a test run leaves its leftovers inside the
    /// build directory and never in the user's temp or home (the recorder's own convention:
    /// `LOCALPLAY_TEST_CLIPS_DIR` also puts a run's recording under `target/`).
    fn temp_dir(what: &str) -> tempfile::TempDir {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/recorder-session-tests");
        std::fs::create_dir_all(&base).expect("creating the test scratch root");
        tempfile::Builder::new()
            .prefix(what)
            .tempdir_in(&base)
            .expect("a temp dir under target/")
    }

    fn store() -> Store {
        let store = Store::open_in_memory().expect("an in-memory index");
        store.migrate().expect("migrating it");
        store
    }

    /// A `sessions` row for a recording that is (or was) running over `dir`.
    fn running_session(store: &Store, dir: &Path, mode: &str) -> i64 {
        store
            .start_session(Some("Dota 2"), mode, 1_700_000_000_000, &dir.display().to_string())
            .expect("opening a session row")
    }

    /// Both lossless concatenation paths keep **every** audio stream.
    ///
    /// This is the regression test for a measured data-loss bug.
    /// [`localplay_media::edit::concat_lossless`] passed no `-map`, so ffmpeg applied its
    /// default stream selection and kept one audio stream: a clip cut from a session that
    /// was recorded with a microphone came out with the voice track missing — exit status 0,
    /// nothing logged, a file the user believes is whole.
    ///
    /// The splicer and `finalise` now share one `-map 0` invocation, so both must keep both
    /// tracks. If a future ffmpeg changed its default selection, or someone dropped the
    /// `-map`, the two halves of this test would disagree and point at which path regressed.
    #[test]
    fn every_lossless_concat_path_keeps_a_second_audio_track() {
        let dir = temp_dir("session-");
        let segments = write_segments(&dir.path().join("session-1"), 2);
        assert_eq!(segments.len(), 2, "two segments");

        // The splicer: what a clip out of a session is cut with.
        let spliced = dir.path().join("spliced.mp4");
        let window = localplay_replay::window::SegmentWindow {
            segments: segments.clone(),
            truncated_front: false,
        };
        let meta = ClipSplicer::splice(&bin(), &window, &spliced, "libx264").expect("the splice");
        assert_eq!(
            audio_stream_count(&bin(), &spliced),
            Some(2),
            "the splicer must keep the microphone track as well as the game audio"
        );

        // The finalise: both tracks, and the file is as long as the two segments.
        let out = dir.path().join("session.mp4");
        let meta2 = finalise(&bin(), &out, &segments, "libx264", true).expect("the finalise");
        assert_eq!(audio_stream_count(&bin(), &out), Some(2), "both tracks survive");
        let info = MediaInfo::probe(&bin(), &out).expect("probing the session file");
        assert!(info.video.is_some(), "the video stream is there");
        assert!(
            info.duration_ms >= 1_900,
            "two one-second segments make a ~2s session, got {}ms",
            info.duration_ms
        );
        assert_eq!(meta2.size_bytes, std::fs::metadata(&out).unwrap().len());
        assert!(meta.size_bytes > 0);

        // The clip must also be as long as the segments it was built from, not merely carry
        // the right number of streams: a concat that kept both audio streams but truncated
        // one would pass the count above.
        let spliced_info = MediaInfo::probe(&bin(), &spliced).expect("probing the clip");
        assert!(
            spliced_info.duration_ms >= 1_900,
            "the spliced clip must be ~2s of media, got {}ms",
            spliced_info.duration_ms
        );
    }

    /// Without a microphone, `finalise` goes through the same `-c copy` machinery and the
    /// file has exactly the streams the segments had.
    #[test]
    fn a_single_track_session_concatenates_to_one_video_and_one_audio_stream() {
        let dir = temp_dir("session-");
        let segments = write_segments(&dir.path().join("session-1"), 1);
        let out = dir.path().join("session.mp4");

        let meta = finalise(&bin(), &out, &segments, "libx264", false).expect("the finalise");

        assert!(out.is_file());
        assert_eq!(meta.size_bytes, std::fs::metadata(&out).unwrap().len());
        let info = MediaInfo::probe(&bin(), &out).expect("probing it");
        assert!(info.video.is_some() && info.audio.is_some());
        assert_eq!(audio_stream_count(&bin(), &out), Some(1));
        assert!(meta.duration_ms >= 1_900, "{}ms for two 1s segments", meta.duration_ms);

        // And a finalise with nothing to concatenate is an error, not an empty file.
        let err = finalise(&bin(), &dir.path().join("empty.mp4"), &[], "libx264", false)
            .expect_err("no segments is not a session");
        assert!(err.to_string().contains("no segments"), "{err}");
    }

    /// The live scan does not trust the newest segment (ffmpeg is still appending to it);
    /// the shutdown scan takes every file, because by then the child has exited.
    #[test]
    fn the_newest_segment_is_taken_at_shutdown_and_not_before_it() {
        let dir = temp_dir("session-");
        let session_dir = dir.path().join("session-1");
        write_segments(&session_dir, 1);

        let mut ring = SessionRing::open(
            &bin(),
            &session_dir,
            &session_cfg(&dir.path().join("clips")),
            false,
            "libx264".into(),
        )
        .expect("opening the session ring");
        assert_eq!(ring.adopt_existing().expect("adopting"), 1, "one complete segment");
        assert_eq!(ring.segment_count(), 1);
        assert_eq!(ring.span_ms(), 1_000, "one second of completed footage");
        ring.scan().expect("scanning again changes nothing");
        assert_eq!(ring.segment_count(), 1, "the newest file is still being written");

        ring.scan_all().expect("the shutdown scan");
        assert_eq!(ring.segment_count(), 2, "at shutdown the newest segment counts");
        assert_eq!(ring.span_ms(), 2_000);
        assert_eq!(ring.bytes_on_disk(), segments_on_disk(&session_dir, 1_000).unwrap().iter().map(|s| s.bytes).sum::<u64>());
        assert_eq!(
            ring.reserve_number().unwrap(),
            2,
            "segments 0 and 1 are on disk, so the encoder must continue at 2 (a fresh \
             directory would number from 0)"
        );
    }

    /// A session directory that already holds segments cannot be written over: the encoder
    /// is told to continue the numbering (the same rule the replay ring follows).
    #[test]
    fn a_session_directory_that_already_holds_segments_continues_the_numbering() {
        let dir = temp_dir("session-");
        let session_dir = dir.path().join("session-1");
        write_segments(&session_dir, 1);

        let mut ring = SessionRing::open(
            &bin(),
            &session_dir,
            &session_cfg(&dir.path().join("clips")),
            false,
            "libx264".into(),
        )
        .expect("opening");
        ring.adopt_existing().expect("adopting");
        assert_eq!(ring.reserve_number().expect("reserving"), 2, "segments 0 and 1 exist");
        assert_eq!(
            ring.span_ms(),
            0,
            "an adopted directory's footage is not this run's timeline (it has captured nothing)"
        );
    }

    /// A crash leaves a running row and a directory of segments; the next start concatenates
    /// them into the file the session would have written, closes the row, removes the
    /// segments — and reports it.
    #[test]
    fn a_crashed_session_is_recovered_into_one_file_and_closed() {
        let dir = temp_dir("session-");
        let sessions_dir = dir.path().join("sessions");
        let session_dir = sessions_dir.join("session-1700000000");
        let segments = write_segments(&session_dir, 1);
        let bytes: u64 = segments.iter().map(|s| s.bytes).sum();
        let store = store();
        let id = running_session(&store, &session_dir, SESSION_MODE_SESSION);
        store.set_session_size(id, bytes as i64).expect("the running size");

        let report = recover_sessions(&store, &sessions_dir, &bin(), 1_000, "libx264");

        assert_eq!(report.recovered, vec![id], "the crashed session was finalised: {report:?}");
        let row = store.get_session(id).unwrap().expect("the row is still there");
        let final_path = row.final_path.expect("the row names the session file");
        assert!(Path::new(&final_path).is_file(), "{final_path} must exist");
        assert!(row.ended_at_ms.is_some(), "and the row is closed");
        assert!(row.size_bytes > 0, "with the real size: {}", row.size_bytes);
        assert_eq!(row.size_bytes as u64, std::fs::metadata(&final_path).unwrap().len());
        assert_eq!(
            final_path,
            session_file_path(&sessions_dir, row.started_at_ms).display().to_string(),
            "the file is named from the session's own start instant"
        );
        assert_eq!(audio_stream_count(&bin(), Path::new(&final_path)), Some(1));
        assert!(!session_dir.exists(), "the temporary segments are gone");

        // A second pass has nothing to recover, and must not touch the finished session.
        let again = recover_sessions(&store, &sessions_dir, &bin(), 1_000, "libx264");
        assert!(again.is_empty(), "nothing left to do: {again:?}");
        assert!(store.get_session(id).unwrap().unwrap().final_path.is_some());
    }

    /// When the concatenation cannot be done (a full disk is the real case; corrupt
    /// segments stand in for it), nothing is lost and the next start tries again: every
    /// segment is kept, and the row is left **running**, which is what keeps the retention
    /// rules from evicting footage that has no session file.
    #[test]
    fn a_recovery_that_cannot_concatenate_keeps_the_segments_and_leaves_the_row_running() {
        let dir = temp_dir("session-");
        let sessions_dir = dir.path().join("sessions");
        let session_dir = sessions_dir.join("session-1700000000");
        std::fs::create_dir_all(&session_dir).unwrap();
        // Not a media file at all: ffmpeg will refuse it, which is the failure path a full
        // disk takes as well (a write that cannot complete).
        std::fs::write(session_dir.join("seg-000000.mp4"), b"this is not an mp4").unwrap();
        let store = store();
        let id = running_session(&store, &session_dir, SESSION_MODE_SESSION);

        let report = recover_sessions(&store, &sessions_dir, &bin(), 1_000, "libx264");

        assert_eq!(report.unfinalised, vec![id], "{report:?}");
        assert!(report.recovered.is_empty());
        assert!(session_dir.join("seg-000000.mp4").is_file(), "the segments are kept");
        let row = store.get_session(id).unwrap().unwrap();
        assert!(
            row.ended_at_ms.is_none(),
            "the row is left running on purpose: a running session is never evicted"
        );
        assert!(row.final_path.is_none());
        assert!(
            !session_file_path(&sessions_dir, row.started_at_ms).exists(),
            "and no partial file is left behind"
        );
    }

    /// A row for a recording that wrote nothing is closed; a buffer-mode row left running by
    /// a crash is closed without touching its scratch directory (the ring owns that); and a
    /// directory no row names is reported rather than deleted.
    #[test]
    fn recovery_closes_empty_rows_and_reports_directories_it_cannot_own() {
        let dir = temp_dir("session-");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // A session row whose directory is empty (a crash between the row and the first
        // segment), and a buffer row whose scratch directory the ring manages.
        let empty_dir = sessions_dir.join("session-1");
        std::fs::create_dir_all(&empty_dir).unwrap();
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("seg-000000.mp4"), b"the ring's own business").unwrap();
        let store = store();
        let empty = running_session(&store, &empty_dir, SESSION_MODE_SESSION);
        let buffered = running_session(&store, &scratch, SESSION_MODE_BUFFER);

        // A directory nobody names.
        let orphan = sessions_dir.join("session-nobody");
        write_segments(&orphan, 1);

        let report = recover_sessions(&store, &sessions_dir, &bin(), 1_000, "libx264");

        let mut closed = report.closed_empty.clone();
        closed.sort_unstable();
        let mut expected = vec![empty, buffered];
        expected.sort_unstable();
        assert_eq!(closed, expected, "{report:?}");
        assert_eq!(report.orphan_dirs, vec![orphan.clone()], "{report:?}");
        assert!(store.get_session(empty).unwrap().unwrap().ended_at_ms.is_some());
        assert!(store.get_session(buffered).unwrap().unwrap().ended_at_ms.is_some());
        assert!(!empty_dir.exists(), "an empty session directory is removed");
        assert!(
            scratch.join("seg-000000.mp4").is_file(),
            "a buffer session's scratch directory is the ring's, and is never touched"
        );
        assert!(orphan.is_dir(), "an unnamed directory is reported, never deleted");
    }

    /// A finished session whose segments are still there (a crash between the concatenation
    /// and the directory's removal) is reported and left alone: the file exists and the row
    /// names it, so the segments are a duplicate the retention pass removes with the row.
    #[test]
    fn a_finished_session_with_leftover_segments_is_reported_not_deleted() {
        let dir = temp_dir("session-");
        let sessions_dir = dir.path().join("sessions");
        let session_dir = sessions_dir.join("session-1700000000");
        write_segments(&session_dir, 1);
        let store = store();
        let id = running_session(&store, &session_dir, SESSION_MODE_SESSION);
        let file = session_file_path(&sessions_dir, 1_700_000_000_000);
        std::fs::write(&file, b"a finished session").unwrap();
        store
            .end_session(id, 1_700_000_001_000, Some(&file.display().to_string()), 20)
            .expect("ending it");

        let report = recover_sessions(&store, &sessions_dir, &bin(), 1_000, "libx264");

        assert_eq!(report.leftover_dirs, vec![session_dir.clone()], "{report:?}");
        assert!(report.recovered.is_empty() && report.unfinalised.is_empty());
        assert!(session_dir.is_dir(), "reported, not removed");
        assert!(report.orphan_dirs.is_empty(), "the row names it, so it is not an orphan");
    }

    /// Two recordings starting in the same second get different directories: the name is
    /// claimed by creating it, so nothing can mix two sessions' segments into one file.
    #[test]
    fn a_session_directory_is_created_and_never_shared() {
        let dir = temp_dir("session-");
        let sessions = dir.path().join("sessions");

        let first = create_session_dir(&sessions, 1_700_000_000_000).expect("the first");
        let second = create_session_dir(&sessions, 1_700_000_000_000).expect("the second");

        assert_ne!(first, second);
        assert!(first.is_dir() && second.is_dir());
        assert_eq!(
            first.file_name().unwrap(),
            "session-1700000000",
            "named from the session's start instant, like its file"
        );
        assert_eq!(
            session_file_path(&sessions, 1_700_000_000_000).file_name().unwrap(),
            "session-1700000000.mp4"
        );
    }
}
