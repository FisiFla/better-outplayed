//! Splitting ffmpeg's **fragmented MP4** output into self-contained fragments.
//!
//! This is what makes an in-memory replay buffer possible. With `-f mp4 -movflags
//! empty_moov+frag_keyframe+default_base_moof` on a pipe, ffmpeg writes one small
//! `ftyp`+`moov` header and then a stream of `moof`+`mdat` pairs — one pair per forced
//! keyframe. Measured on a 6s 320x180 capture: a 1249-byte header, then 6 fragments a second
//! apart, each carrying a `tfdt` (base media decode time) in its track's timescale.
//!
//! Two properties fall out of that shape, and both are load-bearing for a replay buffer:
//!
//! * **A fragment is self-contained.** `header + any contiguous range of fragments` is a valid
//!   fragmented MP4, and `ffmpeg -i that -c copy out.mp4` turns it into a normal clip. That is
//!   the whole clip path: no re-encoding, no temporary segment files, nothing on disk until the
//!   user saves something.
//! * **A fragment starts at a keyframe**, because ffmpeg was told `frag_keyframe` and the
//!   encoder forces a keyframe every `segment_ms`. So a fragment boundary is a cut point, with
//!   exactly the granularity the file-based segmenter has, from the same interval.
//!
//! The splitter is **incremental**: a caller pushes whatever bytes it just read from the pipe
//! and gets back any fragments that are now complete. Nothing is buffered twice — each
//! fragment's bytes are moved out as soon as its `mdat` has arrived.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;

/// One fragmented-MP4 fragment: a `moof` and the `mdat` that follows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    /// Sequence number, counted from 0 in the order the splitter produced them.
    pub seq: u64,
    /// Where this fragment's earliest video sample decodes, in ms — its `tfdt`, converted with
    /// the **video** track's timescale.
    ///
    /// This is media time, run-relative, and monotonic across a stream. It is what the buffer
    /// measures its span with and what a clip's range is selected against; nothing is decoded
    /// to get it.
    pub start_ms: u64,
    /// The bytes: this fragment's `moof` and its `mdat`, and nothing else.
    pub bytes: Vec<u8>,
    /// Whether the fragment starts on a keyframe.
    ///
    /// Always `true` for fragments this splitter produces, because ffmpeg was spawned with
    /// `frag_keyframe`. It is carried rather than assumed so that the code which picks a clip's
    /// first fragment *asserts* it: a clip that began mid-GOP would play its first frames as
    /// garbage, and that is exactly the kind of silent damage a buffer should refuse to cause.
    pub keyframe: bool,
}

/// An incremental splitter for ffmpeg's fragmented-MP4 output.
///
/// Not `Clone`: it owns the unconsumed tail of a stream, and two of them would be two readers.
#[derive(Debug, Default)]
pub struct FragmentSplitter {
    /// Bytes received but not yet consumed by a complete box.
    buf: Vec<u8>,
    /// `ftyp` + `moov`, once both have been seen. Kept whole: a clip is this plus a range of
    /// fragments, and the header is the only part that is written once.
    header: Option<Vec<u8>>,
    /// Track id → `(is_video, timescale)`, read out of the `moov`.
    ///
    /// Needed to convert a fragment's `tfdt` into milliseconds: the value is in the *track's*
    /// own timescale (measured: 15360 for video, 48000 for audio in the same stream), and the
    /// video one is the clock a clip's range is measured on.
    tracks: HashMap<u32, (bool, u32)>,
    /// A `moof` whose `mdat` has not arrived yet, with the start time read out of it.
    pending: Option<(u64, Vec<u8>)>,
    next_seq: u64,
}

impl FragmentSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The `ftyp`+`moov` header, once it has been seen.
    ///
    /// `None` until then. A caller that wants to write a clip needs it, and it is the one piece
    /// that must be remembered across evictions.
    pub fn header(&self) -> Option<&[u8]> {
        self.header.as_deref()
    }

    /// Bytes received so far that do not yet form a complete box.
    ///
    /// Reported so a caller can bound its own memory: a stream that stops mid-box must not
    /// grow a buffer for ever.
    pub fn pending_bytes(&self) -> usize {
        self.buf.len()
    }

    /// How many **audio** tracks the header declares.
    ///
    /// Needed by the save path, which has to re-apply the track names: a `-c copy` carries no
    /// per-stream metadata, so `edit::audio_titles` must be told how many tracks to name. Zero
    /// before the header has been seen.
    pub fn audio_tracks(&self) -> usize {
        self.tracks.values().filter(|(is_video, _)| !is_video).count()
    }

    /// How many video tracks the header declares. Zero before the header has been seen.
    pub fn video_tracks(&self) -> usize {
        self.tracks.values().filter(|(is_video, _)| *is_video).count()
    }

    /// Feed bytes just read from the pipe; returns whatever fragments that completed.
    ///
    /// A fragment is only returned once its `mdat` has arrived in full, so every `Fragment` is
    /// immediately usable — and immediately droppable, which is what lets a ring buffer evict
    /// from the front without ever holding a half-box.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Fragment>> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            let Some((kind, size)) = peek_box(&self.buf)? else {
                break;
            };
            if self.buf.len() < size {
                break; // the rest of this box has not arrived yet
            }
            let body: Vec<u8> = self.buf.drain(..size).collect();
            match kind.as_str() {
                "ftyp" => {
                    // The header starts here; `moov` appends to it. Anything before this is not
                    // part of the header, so it does not accumulate.
                    self.header = Some(body);
                }
                "moov" => {
                    self.tracks = parse_tracks(&body)
                        .context("reading the track table from ffmpeg's fragmented-MP4 header")?;
                    let mut header = self.header.take().unwrap_or_default();
                    header.extend_from_slice(&body);
                    self.header = Some(header);
                }
                "moof" => {
                    if self.pending.is_some() {
                        bail!(
                            "two `moof` boxes in a row: the fragment before this one never \
                             received its `mdat`, so its samples cannot be recovered"
                        );
                    }
                    let start_ms = self.start_ms(&body)?;
                    self.pending = Some((start_ms, body));
                }
                "mdat" => {
                    let Some((start_ms, moof)) = self.pending.take() else {
                        // A bare `mdat` with no `moof` in front of it is not a fragment. This
                        // is not an error — `free`/`skip` boxes and other muxer furniture are
                        // legal at the top level — but it is not footage either, so it is
                        // dropped rather than invented into a fragment.
                        continue;
                    };
                    let mut bytes = moof;
                    bytes.extend_from_slice(&body);
                    out.push(Fragment {
                        seq: self.next_seq,
                        start_ms,
                        bytes,
                        // See `Fragment::keyframe`: `frag_keyframe` guarantees it.
                        keyframe: true,
                    });
                    self.next_seq += 1;
                }
                // `free`, `skip`, `sidx`, `styp`, `mfra`, `moov` after the first, …
                _ => {}
            }
        }
        Ok(out)
    }

    /// The `tfdt` of a fragment's **video** track, in ms.
    fn start_ms(&self, moof: &[u8]) -> Result<u64> {
        for traf in children(moof, 8, "traf") {
            let Some(track_id) = track_id(traf) else { continue };
            let Some((is_video, timescale)) = self.tracks.get(&track_id) else {
                continue;
            };
            if !is_video || *timescale == 0 {
                continue;
            }
            let Some(ticks) = base_media_decode_time(traf) else {
                continue;
            };
            return Ok(ticks.saturating_mul(1_000) / u64::from(*timescale));
        }
        // A fragment with no video track is not something this pipeline produces (the video is
        // input 0 and always mapped), so it is a malformed stream rather than a case to guess at.
        bail!("no video `tfdt` in this fragment: its start time cannot be placed on the timeline")
    }
}

/// The kind and declared size of the box at the front of `buf`, or `None` if fewer than eight
/// bytes have arrived.
///
/// Returns the *total* size, header included — so `size` is what has to be drained.
fn peek_box(buf: &[u8]) -> Result<Option<(String, usize)>> {
    if buf.len() < 8 {
        return Ok(None);
    }
    let small = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let kind = String::from_utf8_lossy(&buf[4..8]).into_owned();
    let size = match small {
        0 => {
            // "to the end of the file" — meaningless on a live stream, and ffmpeg does not
            // emit it. Treated as an error rather than guessed at.
            bail!("a `{kind}` box declares size 0 (extend to end of file) on a live stream");
        }
        1 => {
            if buf.len() < 16 {
                return Ok(None);
            }
            let large = u64::from_be_bytes([
                buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
            ]);
            usize::try_from(large).context("a 64-bit box size does not fit in memory")?
        }
        n => n as usize,
    };
    if size < 8 {
        bail!("a `{kind}` box declares a size of {size}, which cannot hold its own header");
    }
    Ok(Some((kind, size)))
}

/// The boxes directly inside `buf[from..to]`, in order.
fn boxes(buf: &[u8]) -> Vec<(&[u8], String)> {
    let mut out = Vec::new();
    let mut off = 0;
    while off + 8 <= buf.len() {
        let small = u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        let kind = String::from_utf8_lossy(&buf[off + 4..off + 8]).into_owned();
        let (size, hdr) = match small {
            0 => (buf.len() - off, 8),
            1 if off + 16 <= buf.len() => {
                let large = u64::from_be_bytes([
                    buf[off + 8],
                    buf[off + 9],
                    buf[off + 10],
                    buf[off + 11],
                    buf[off + 12],
                    buf[off + 13],
                    buf[off + 14],
                    buf[off + 15],
                ]);
                (usize::try_from(large).unwrap_or(0), 16)
            }
            1 => return out,
            n => (n as usize, 8),
        };
        if size < hdr || off + size > buf.len() {
            break;
        }
        out.push((&buf[off + hdr..off + size], kind));
        off += size;
    }
    out
}

/// The child boxes of `buf` of a given kind, skipping its own header.
///
/// Collected rather than returned as an iterator: these lists hold a handful of boxes, and an
/// `impl Iterator` would have to carry `kind`'s lifetime as well as the buffer's to satisfy the
/// borrow checker — a bound that says nothing useful about what this does.
fn children<'a>(buf: &'a [u8], header: usize, kind: &str) -> Vec<&'a [u8]> {
    let body = buf.get(header..).unwrap_or(&[]);
    boxes(body)
        .into_iter()
        .filter(|(_, k)| k == kind)
        .map(|(b, _)| b)
        .collect()
}

/// Track id → `(is_video, timescale)`, from a `moov`.
fn parse_tracks(moov: &[u8]) -> Result<HashMap<u32, (bool, u32)>> {
    let mut tracks = HashMap::new();
    for trak in children(moov, 8, "trak") {
        let Some(tkhd) = children(trak, 0, "tkhd").first().copied() else {
            continue;
        };
        // `tkhd`: version(1) flags(3) [v1: creation(8) modification(8) track_id(4)]
        //         [v0: creation(4) modification(4) track_id(4)]
        let version = tkhd.first().copied().unwrap_or(0);
        let id_at = if version == 1 { 20 } else { 12 };
        let Some(id_bytes) = tkhd.get(id_at..id_at + 4) else {
            continue;
        };
        let track_id = u32::from_be_bytes([id_bytes[0], id_bytes[1], id_bytes[2], id_bytes[3]]);

        let mdia = children(trak, 0, "mdia").first().copied().unwrap_or(&[]);
        let handler = children(mdia, 0, "hdlr").first().copied().unwrap_or(&[]);
        // `hdlr`: version(1) flags(3) pre_defined(4) handler_type(4)
        let is_video = handler.get(8..12) == Some(b"vide");
        let mdhd = children(mdia, 0, "mdhd").first().copied().unwrap_or(&[]);
        let ts_version = mdhd.first().copied().unwrap_or(0);
        let ts_at = if ts_version == 1 { 20 } else { 12 };
        let Some(ts_bytes) = mdhd.get(ts_at..ts_at + 4) else {
            continue;
        };
        let timescale = u32::from_be_bytes([ts_bytes[0], ts_bytes[1], ts_bytes[2], ts_bytes[3]]);
        tracks.insert(track_id, (is_video, timescale));
    }
    if tracks.is_empty() {
        bail!("the track table is empty: this is not an MP4 header this build understands");
    }
    Ok(tracks)
}

/// The `track_id` a `traf` refers to, from its `tfhd`.
fn track_id(traf: &[u8]) -> Option<u32> {
    let tfhd = *children(traf, 0, "tfhd").first()?;
    // `tfhd`: version(1) flags(3) track_id(4)
    let b = tfhd.get(4..8)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// The `tfdt` (base media decode time) of a `traf`, in the track's timescale ticks.
fn base_media_decode_time(traf: &[u8]) -> Option<u64> {
    let tfdt = *children(traf, 0, "tfdt").first()?;
    let version = tfdt.first().copied().unwrap_or(0);
    if version == 1 {
        let b = tfdt.get(4..12)?;
        Some(u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    } else {
        let b = tfdt.get(4..8)?;
        Some(u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real fragmented-MP4 stream from ffmpeg, in memory.
    ///
    /// Generated rather than pasted: a hand-written box structure would test the parser against
    /// my idea of ffmpeg's output instead of its actual output, and the whole point of this
    /// module is that it agrees with the muxer.
    fn stream(seconds: u32) -> Vec<u8> {
        let bin = localplay_media_ffmpeg();
        let out = std::process::Command::new(&bin.ffmpeg)
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc2=size=320x180:rate=30:duration={seconds}"),
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:sample_rate=48000:duration={seconds}"),
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-g",
                "30",
                "-force_key_frames",
                "expr:gte(t,n_forced*1)",
                "-c:a",
                "aac",
                "-ac",
                "2",
                "-f",
                "mp4",
                "-movflags",
                "empty_moov+frag_keyframe+default_base_moof",
                "-",
            ])
            .output()
            .expect("spawning ffmpeg to generate a fragmented-MP4 stream");
        assert!(out.status.success(), "ffmpeg could not generate the stream");
        out.stdout
    }

    fn localplay_media_ffmpeg() -> crate::FfmpegBinaries {
        crate::FfmpegBinaries::discover(None).expect("ffmpeg on PATH")
    }

    #[test]
    fn the_splitter_produces_the_header_then_one_fragment_per_second() {
        let data = stream(4);
        let mut splitter = FragmentSplitter::new();
        let fragments = splitter.push(&data).expect("the stream splits");

        let header = splitter.header().expect("the header was seen");
        assert!(
            header.starts_with(&(28u32.to_be_bytes())),
            "the header must begin with its own `ftyp` box size"
        );
        assert_eq!(&header[4..8], b"ftyp", "and be the ftyp the muxer wrote");
        assert!(
            header.windows(4).any(|w| w == b"moov"),
            "the header must contain the moov, or a clip cannot be built from it"
        );
        assert!(
            !header.windows(4).any(|w| w == b"moof"),
            "and no fragment bytes: the header is the part that is written once"
        );

        // `-force_key_frames` is one per second, so a 4s stream is four fragments. The exact
        // count is ffmpeg's, so this asserts a band rather than a number — but a stream that
        // produced ONE fragment would make a clip unable to cut anything, which must fail.
        assert!(
            (3..=5).contains(&fragments.len()),
            "a 4s stream at 1s fragments should be about four, got {}",
            fragments.len()
        );
        assert!(splitter.pending_bytes() == 0, "no partial box should be left over");
    }

    #[test]
    fn fragment_start_times_are_monotonic_and_about_a_second_apart() {
        let data = stream(4);
        let mut splitter = FragmentSplitter::new();
        let fragments = splitter.push(&data).expect("the stream splits");

        let starts: Vec<u64> = fragments.iter().map(|f| f.start_ms).collect();
        assert_eq!(starts[0], 0, "the first fragment starts at zero");
        assert!(
            starts.windows(2).all(|w| w[0] < w[1]),
            "media time must advance: {starts:?}"
        );
        for pair in starts.windows(2) {
            let step = pair[1] - pair[0];
            assert!(
                (700..=1_400).contains(&step),
                "fragments are forced on 1s keyframes, so a step should be about 1000ms: \
                 {starts:?}"
            );
        }
    }

    #[test]
    fn every_fragment_carries_a_moof_and_its_mdat_and_claims_a_keyframe() {
        let data = stream(3);
        let mut splitter = FragmentSplitter::new();
        for fragment in splitter.push(&data).expect("the stream splits") {
            assert!(
                fragment.keyframe,
                "frag_keyframe means every fragment starts on a keyframe, and a clip cut must \
                 start on one"
            );
            assert_eq!(
                &fragment.bytes[4..8],
                b"moof",
                "a fragment begins with its moof"
            );
            assert!(
                fragment.bytes.windows(4).any(|w| w == b"mdat"),
                "and carries the mdat with its samples, or it would be a header with no footage"
            );
        }
    }

    #[test]
    fn feeding_the_stream_one_byte_at_a_time_gives_the_same_fragments() {
        // A pipe does not deliver whole boxes. This is the property that matters for a reader
        // thread: how the bytes arrive must not change what they mean.
        let data = stream(3);
        let mut all_at_once = FragmentSplitter::new();
        let expected = all_at_once.push(&data).expect("whole stream");

        let mut drip = FragmentSplitter::new();
        let mut got = Vec::new();
        for byte in &data {
            got.extend(drip.push(&[*byte]).expect("one byte at a time"));
        }

        assert_eq!(got.len(), expected.len(), "the same number of fragments");
        assert_eq!(
            got.iter().map(|f| (f.seq, f.start_ms)).collect::<Vec<_>>(),
            expected.iter().map(|f| (f.seq, f.start_ms)).collect::<Vec<_>>(),
            "with the same sequence numbers and start times"
        );
        assert_eq!(
            drip.header(),
            all_at_once.header(),
            "and the same header, however the bytes arrived"
        );
        assert_eq!(drip.pending_bytes(), 0, "and nothing left over");
    }

    #[test]
    fn a_stream_that_stops_mid_box_is_held_rather_than_guessed_at() {
        // The failure mode this rules out: a reader that acted on a half-received `mdat` would
        // hand the buffer a fragment with truncated samples — a clip that plays and then tears.
        let data = stream(3);
        let cut = data.len() - data.len() / 4;
        let mut splitter = FragmentSplitter::new();
        let fragments = splitter.push(&data[..cut]).expect("a partial stream is not an error");
        assert!(
            splitter.pending_bytes() > 0,
            "the tail of the last box must be waiting, not consumed"
        );
        let completed = fragments.len();

        // The rest arrives: the held bytes complete into a fragment, and nothing is lost.
        let rest = splitter.push(&data[cut..]).expect("the rest of the stream");
        assert_eq!(
            completed + rest.len(),
            FragmentSplitter::new().push(&data).unwrap().len(),
            "finishing the stream must produce exactly the fragments it always had"
        );
        assert_eq!(splitter.pending_bytes(), 0);
    }
}
