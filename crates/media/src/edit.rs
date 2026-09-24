//! Lossless (stream-copy) media edits. These never re-encode.

use crate::binaries::{run_with_stdin, run_with_timeout, FfmpegBinaries};
use crate::probe::{stream_layout, StreamLayout};
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const EDIT_TIMEOUT: Duration = Duration::from_secs(60);

/// The floor a lossless copy is given, on top of the time its own size implies.
pub const COPY_FLOOR: Duration = Duration::from_secs(60);

/// The throughput the size-derived part of a copy's budget assumes, in bytes/second.
///
/// A stream copy is I/O bound: ffmpeg reads every byte and writes every byte, so the wall
/// time a concatenation needs scales with the *bytes* it moves, not with the media
/// duration. A fixed budget is therefore wrong in both directions — far too generous for a
/// 30-second clip, nowhere near enough for a multi-gigabyte session, and it is the second
/// one that hurts, because the copy fails only *after* doing all the work.
///
/// Deliberately pessimistic (10 MB/s), because the two errors are not symmetric: too large
/// costs a hung ffmpeg that the caller then kills, too small costs a concatenation that
/// fails after the work. A slow spinning disk still clears 10 MB/s.
pub const COPY_MIN_BYTES_PER_SEC: u64 = 10_000_000;

/// Wall-clock budget for copying `bytes` with `-c copy`.
pub fn copy_budget(bytes: u64) -> Duration {
    COPY_FLOOR + Duration::from_secs(bytes / COPY_MIN_BYTES_PER_SEC.max(1))
}

/// Remux without re-encoding, moving the index to the front for fast seeking.
pub fn remux_lossless(bin: &FfmpegBinaries, src: &Path, dst: &Path) -> Result<()> {
    let mut cmd = Command::new(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-i"])
        .arg(src)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(dst);
    expect_success(cmd, "remux", dst, EDIT_TIMEOUT)
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
    expect_success(cmd, "trim", dst, EDIT_TIMEOUT)
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
    expect_success(cmd, "thumbnail", dst, EDIT_TIMEOUT)
}

/// Concatenate segments into one file with `-c copy`.
///
/// Every segment must already share identical codec parameters — they do, because
/// they come from one encoder invocation (spec §6.1).
///
/// **`-map 0` is what makes this correct, and its absence was silent data loss.** With no
/// `-map`, ffmpeg applies its default stream selection, which keeps exactly one audio
/// stream. Measured on ffmpeg 9.0.2: concatenating segments that each carry 1 video + 2
/// audio streams produced a 2-stream file — the microphone track vanished from a clip the
/// user recorded with one, with no error and exit status 0.
pub fn concat_lossless(bin: &FfmpegBinaries, list_file: &Path, dst: &Path) -> Result<()> {
    // One audio stream: the ordinary case for a recording with no microphone. A caller that
    // concatenates a two-track recording must use [`concat_lossless_sized`] with the real count,
    // or the microphone's track would come out named "Game Audio".
    concat_lossless_sized(bin, list_file, dst, 0, 1)
}

/// Remux a **fragmented-MP4 byte stream** into an ordinary clip file with `-c copy`.
///
/// This is how a clip is saved out of an in-memory ring. The footage has only ever been in RAM,
/// and the file written here — named by the caller — is the **only** thing that touches the disk
/// during the whole buffer-and-save cycle.
///
/// `-i pipe:0`, deliberately: spilling the stream to a temporary file and remuxing *that* would
/// reintroduce exactly the scratch churn this path exists to remove, at the one moment it is
/// least forgivable — the user has pressed the key and is waiting for the clip.
/// [`run_with_stdin`] writes the bytes on its own thread for the same reason the encoder does: a
/// pipe's buffer is 64KiB here and as little as 4KiB for a Windows anonymous pipe, so a blocking
/// write on this thread could stall before the deadline bounding it ever ran.
///
/// The audio tracks are re-named from [`audio_titles`], because a `-c copy` to a file carries no
/// per-stream metadata: without this the clip's tracks come out anonymous, which is a defect
/// measured on real hardware and fixed the same way in the file concat.
pub fn remux_stream_lossless(
    bin: &FfmpegBinaries,
    bytes: Vec<u8>,
    dst: &Path,
    audio_streams: usize,
) -> Result<()> {
    if bytes.is_empty() {
        bail!("refusing to remux an empty stream: there is no footage to write");
    }
    // The budget is derived from the bytes actually being moved, like the concat's: this is the
    // same copy, from a pipe instead of a list of files.
    let budget = copy_budget(bytes.len() as u64);
    let mut cmd = Command::new(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-i", "pipe:0", "-map", "0", "-c", "copy"]);
    for (index, title) in audio_titles(audio_streams).iter().enumerate() {
        cmd.arg(format!("-metadata:s:a:{index}")).arg(format!("title={title}"));
    }
    cmd.args(["-movflags", "+faststart"]).arg(dst);

    let out = run_with_stdin(cmd, bytes, budget)?;
    if !out.status.success() {
        bail!(
            "remuxing the in-memory stream into {} failed: {}",
            dst.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if !dst.is_file() {
        bail!(
            "remuxing the in-memory stream reported success but {} does not exist",
            dst.display()
        );
    }
    Ok(())
}

/// As [`concat_lossless`], with the copy's budget derived from the bytes being moved.
///
/// A whole session is the case that needs this: at a few hundred MB the floor is fine, at
/// several GB it is not, and a copy that runs out of time fails only *after* doing the
/// work. `total_bytes` is the summed size of the listed segments; pass 0 for the floor.
/// The `title` the game-audio track is tagged with — the encoder's own string.
pub const GAME_AUDIO_TITLE: &str = "Game Audio";

/// The `title` the microphone track is tagged with — the encoder's own string.
pub const MICROPHONE_TITLE: &str = "Microphone";

/// The titles for a lossless concatenation's audio tracks, in stream order.
///
/// **A `-c copy` through the concat demuxer does not carry per-stream metadata.** Measured: the
/// encoder's own segments *do* carry it — `tags: { "name": "Game Audio" }`, because ffmpeg's MP4
/// muxer stores `title=` in the track's `name` atom — but after `-map 0 -c copy` neither name
/// survives, with or without `-map_metadata 0`. So every clip and every session file made from
/// segments came out with **anonymous** audio tracks: a player showed two tracks both called
/// "Audio", with nothing to say which was the game and which the microphone. Found by probing a
/// real clip produced on Windows (`docs/verification-status.md` §10.2). The concat therefore
/// re-applies them.
///
/// The convention is the encoder's and these are the encoder's strings: it is the thing that
/// decides a recording has the game audio first and the microphone second, so this reproduces
/// that decision rather than inventing a second one. One audio stream is the game audio; two are
/// the game audio plus the microphone. More than two cannot happen — the encoder maps at most
/// two — and any beyond that are left as the muxer named them rather than mislabelled.
pub fn audio_titles(audio_streams: usize) -> Vec<&'static str> {
    match audio_streams {
        0 => Vec::new(),
        1 => vec![GAME_AUDIO_TITLE],
        _ => vec![GAME_AUDIO_TITLE, MICROPHONE_TITLE],
    }
}

/// As [`concat_lossless`], with the copy's budget derived from the bytes being moved, and the
/// audio tracks re-named.
///
/// A whole session is the case that needs the budget: at a few hundred MB the floor is fine, at
/// several GB it is not, and a copy that runs out of time fails only *after* doing the work.
/// `total_bytes` is the summed size of the listed segments; pass 0 for the floor.
///
/// `audio_streams` is how many audio streams the segments carry — the count
/// [`check_concat_layout`] returns — and it changes only the **names**, never the mapping:
/// `-map 0` keeps every stream whatever this says. See [`audio_titles`] for why the names have
/// to be re-applied here at all.
pub fn concat_lossless_sized(
    bin: &FfmpegBinaries,
    list_file: &Path,
    dst: &Path,
    total_bytes: u64,
    audio_streams: usize,
) -> Result<()> {
    let mut cmd = Command::new(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(list_file)
        .args(["-map", "0", "-c", "copy"]);
    for (index, title) in audio_titles(audio_streams).iter().enumerate() {
        cmd.arg(format!("-metadata:s:a:{index}")).arg(format!("title={title}"));
    }
    cmd.args(["-movflags", "+faststart"]).arg(dst);
    expect_success(cmd, "concat", dst, copy_budget(total_bytes))
}

/// Write a concat list naming `segments` in order; returns their total size in bytes.
///
/// The size is returned rather than re-derived by the caller because this list is the
/// authoritative statement of what will be read, and because [`concat_lossless_sized`]
/// needs it for the budget.
pub fn write_concat_list(segments: &[PathBuf], list: &Path) -> Result<u64> {
    let mut file =
        std::fs::File::create(list).with_context(|| format!("creating {}", list.display()))?;
    let mut total = 0u64;
    for seg in segments {
        // Absolute, so the list does not depend on the working directory.
        let path =
            std::fs::canonicalize(seg).with_context(|| format!("resolving {}", seg.display()))?;
        total += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        writeln!(file, "file '{}'", concat_list_path(&path))
            .with_context(|| format!("writing {}", list.display()))?;
    }
    Ok(total)
}

/// Refuse a concatenation whose inputs do not all share one stream layout.
///
/// This is the check that makes `-map 0` safe, and without it `-map 0` only trades one
/// silent failure for another. `-map 0` takes every stream of the **first** input, so the
/// first segment's layout decides the output's, and a later segment that disagrees is then
/// silently wrong. Measured on ffmpeg 9.0.2: with `-map 0`, feeding a 3-stream segment
/// followed by a 2-stream one **exits 0** and still writes the concatenation, and a
/// 1-stream segment in the middle truncates that track — a track stopping at 1.02s inside
/// a 2s file. Neither is an error ffmpeg reports, and both leave the user a file they
/// believe is whole.
///
/// So the layouts are compared first and a mismatch is refused, naming the position and
/// both shapes. The caller keeps its segments and loses only the automatic step, which is
/// the right way round: a visible failure beats an invisible fragment.
pub fn check_concat_layout(bin: &FfmpegBinaries, segments: &[PathBuf]) -> Result<StreamLayout> {
    let Some((first, rest)) = segments.split_first() else {
        bail!("cannot check the stream layout of an empty segment list");
    };
    let expected = stream_layout(bin, first)
        .with_context(|| format!("reading the stream layout of {}", first.display()))?;
    for (i, seg) in rest.iter().enumerate() {
        let got = stream_layout(bin, seg)
            .with_context(|| format!("reading the stream layout of {}", seg.display()))?;
        if got != expected {
            bail!(
                "segment {} of {} differs in stream layout from the first segment, so \
                 concatenating them would silently drop or truncate a track: first is [{}] \
                 ({}) but this one is [{}] ({}). Refused; the segments are left in place",
                i + 2,
                segments.len(),
                describe(&expected),
                expected.counts_summary(),
                describe(&got),
                got.counts_summary(),
            );
        }
    }
    Ok(expected)
}

/// `video/h264 64x48, audio/aac 48000Hz 2ch` — the shapes, for a refusal message.
fn describe(layout: &StreamLayout) -> String {
    layout.streams.iter().map(|s| s.summary()).collect::<Vec<_>>().join(", ")
}

fn expect_success(cmd: Command, what: &str, dst: &Path, budget: Duration) -> Result<()> {
    let out = run_with_timeout(cmd, budget)?;
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

/// Render `path` as the path field of one `file '...'` line of an ffmpeg concat list.
///
/// The concat demuxer parses the list itself, so an OS-native path cannot simply be
/// written out as-is (spec §6.2 step 3). Three things have to change:
///
/// * **Verbatim prefixes.** On Windows `std::fs::canonicalize` returns a *verbatim*
///   path — `\\?\C:\Users\...`, or `\\?\UNC\server\share\...` for a network path.
///   ffmpeg has no notion of that prefix and fails to open what follows, so it is
///   stripped first. The UNC form keeps its meaning by becoming `//server/share/...`.
/// * **Backslashes.** The concat list is parsed as an escaped string, and a backslash
///   sitting next to a quote mangles the field — measured on ffmpeg 9.0.2, where a
///   `file '.../it's dir/...'` line was truncated at the quote and a `\'` spelling
///   resolved to neither. Windows spells the separator `\`, so the separator is
///   converted to `/`, which leaves the parser nothing to escape and which ffmpeg
///   accepts on Windows as well.
/// * **Single quotes.** A quote would end the quoted path early. The demuxer's
///   spelling for a literal quote inside a quoted path is the shell's: `'\''`.
///   (Checked against ffmpeg 9.0.2: `file '.../it'\''s.mp4'` opens the file, while
///   both a bare quote and the `\'` spelling fail.)
///
/// The order is load-bearing: the backslashes are converted *before* the quote
/// escaping, so the backslash the escape itself introduces is not rewritten into a
/// slash (which would leave the path ending at the first quote).
///
/// On Unix a backslash is an ordinary filename byte, so the conversion is a
/// Windows-shaped choice; the paths here are scratch-segment paths generated by this
/// application, none of which contain one.
///
/// This lives here, beside the concat invocation whose list it renders, rather than with
/// any one caller: the replay splicer and the recorder's session finalise both build lists,
/// and a second copy of this escaping is a second place for the Windows handling to be got
/// wrong. [`crate::edit::write_concat_list`] is the only caller that should exist.
pub fn concat_list_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let unverbatim = match raw.strip_prefix(r"\\?\UNC\") {
        // `\\?\UNC\server\share\dir` — a UNC path in verbatim guise.
        Some(rest) => format!("//{rest}"),
        None => raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string(),
    };
    unverbatim.replace('\\', "/").replace('\'', r"'\''")
}

#[cfg(test)]
mod tests {
    use super::{concat_list_path, copy_budget, COPY_FLOOR, COPY_MIN_BYTES_PER_SEC};
    use std::path::Path;
    use std::time::Duration;

    // These paths are never canonicalised here: on the Unix host this runs on, a
    // Windows path is just a string, which is exactly what makes the prefix and
    // backslash handling — the part that breaks on Windows — testable off Windows.

    #[test]
    fn a_verbatim_windows_path_loses_its_prefix_and_its_backslashes() {
        assert_eq!(
            concat_list_path(Path::new(r"\\?\C:\Users\a\videos\seg-000001.mp4")),
            "C:/Users/a/videos/seg-000001.mp4"
        );
    }

    #[test]
    fn a_verbatim_unc_path_becomes_a_plain_unc_path() {
        assert_eq!(
            concat_list_path(Path::new(r"\\?\UNC\server\share\videos\seg-000001.mp4")),
            "//server/share/videos/seg-000001.mp4"
        );
    }

    #[test]
    fn a_plain_unix_path_is_untouched() {
        assert_eq!(concat_list_path(Path::new("/tmp/a/seg.mp4")), "/tmp/a/seg.mp4");
    }

    #[test]
    fn a_single_quote_is_escaped_so_the_list_stays_valid() {
        assert_eq!(
            concat_list_path(Path::new("/tmp/it's a dir/seg.mp4")),
            r"/tmp/it'\''s a dir/seg.mp4"
        );
    }

    #[test]
    fn quote_escaping_survives_backslash_conversion() {
        // The escape `'\''` contains a backslash: if the conversions ran in the other
        // order it would be rewritten to `/` and the demuxer would read the path as
        // ending at the quote.
        let rendered = concat_list_path(Path::new(r"\\?\C:\a b\it's\seg.mp4"));
        assert_eq!(rendered, r"C:/a b/it'\''s/seg.mp4");
        // The only backslashes left are the ones inside the quote escape; any other
        // one would be read by the demuxer as an escape character.
        assert!(
            !rendered.replace(r"'\''", "").contains('\\'),
            "a path separator survived as a backslash: {rendered}"
        );
    }

    #[test]
    fn a_copy_budget_grows_with_the_bytes_and_never_drops_below_the_floor() {
        // The failure this prevents is a multi-gigabyte session copy killed at 60s, so the
        // property that matters is that the budget is never *smaller* than the floor and
        // rises with size.
        assert_eq!(copy_budget(0), COPY_FLOOR, "no bytes means the floor alone");
        assert_eq!(copy_budget(1), COPY_FLOOR, "a byte is not a second");
        assert!(copy_budget(COPY_MIN_BYTES_PER_SEC) > COPY_FLOOR, "10MB buys a second");

        // 4 GB is ~429 seconds of copying on top of the floor. Asserted as a range so the
        // test does not restate the arithmetic it is checking.
        let four_gb = 4_000_000_000u64;
        let budget = copy_budget(four_gb);
        assert!(
            (Duration::from_secs(460)..=Duration::from_secs(500)).contains(&budget),
            "4GB should buy ~429s on top of the 60s floor, got {budget:?}"
        );
    }
}
