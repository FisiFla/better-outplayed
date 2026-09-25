# HEVC for the hardware encode path

A new task, taken on at the user's request after the Tier-2 zero-copy work closed
(`2026-09-25-localplay-tier-2-zero-copy.md`). Written down here because it is the next thing to
build and it should not have to be re-derived from a conversation.

## Why, and what it is not for

HEVC is more efficient **per bit**: the same picture for fewer bits. On this pipeline that means a
smaller RAM ring and smaller clips — concretely, the ring held ~178 MB per 95 s of 4K on the
zero-copy path, so HEVC should roughly halve it, and better quality at the same size.

**It is not a CPU or GPU saving, and the report must say so.** The CPU win that made this path worth
building came from deleting the readback of the captured frame (69.6% of the capture loop's wall
clock, and most of the 88.9% of a core), and that is codec-independent — HEVC cannot remove it a
second time. NVENC's HEVC pass costs slightly *more* per frame than its H.264 one, so GPU use goes
up a little, not down. The pipe that would shrink further is already ~800x smaller than the raw one
(`submit=0.2%`).

## What the box has

The 3090 (Ampere) does H.264 and HEVC through NVENC. It does **not** do AV1 — that is 40-series and
later, so AV1 would need a different GPU. The MFT is found by `MFTEnumEx`; the Intel QSV encoders
are enumerated but report `d3d11=false` and cannot take a D3D11 device, so they are not candidates.

## The changes

1. **The MFT's output type is codec-aware.** `MftEncoder::set_h264_output_type` in
   `crates/encoder/src/mft.rs` hardcodes `MFVideoFormat_H264` with `eAVEncH264VProfile_High` and
   `eAVEncH264VLevel5_1`. HEVC needs `MFVideoFormat_HEVC` with the H265 profile/level equivalents
   (the 3090 reaches 4K at HEVC level 5.1, matching what H.264 needed). Everything else about the
   encoder — the async unlock, the device manager, output-type-before-input, the single
   `FRAME_RATE` constant for both types, the keyframe spacing — is codec-independent and should
   stay as it is.
2. **Carry the codec into the feed.** `MftEncoder` is opened *inside* the encoder thread
   (`mft::encode_loop`), from the first texture, because the device and the size both come from
   that texture. The codec is not in the texture, so it has to be passed to `MftFeed::spawn` and
   held by the thread. `FfmpegEncoder::spawn` knows it — `EncodeConfig` already carries a codec.
3. **`-f hevc`, not `-f h264`.** `video_input_args_bitstream()` in `crates/encoder/src/ffmpeg.rs`
   tells the child what it is reading. `-c:v copy`, the audio, the muxing and the whole
   `VideoInput::EncodedBitstream` arrangement are unaffected.
4. **The config already names a codec.** `encode.codec` is parsed into `VideoCodec`; `"hevc"` has to
   resolve and reach *both* paths — ffmpeg's software encoder for the raw path (`libx265`, and
   `hevc_nvenc`/`hevc_qsv`/`hevc_amf` for the hardware-vendor raw path) and the MFT for the
   zero-copy path.

## Good news found while checking: the selection already generalises

`MFTEnumEx` is called with `None` for the **output**-type filter and `Some(&input)` for the input
one, so it enumerates every hardware video encoder on the machine — H.264, HEVC, and AV1 on a card
that has it — and then `configure_encoder` tries each candidate in turn and keeps the first that
accepts the output type. Nothing selects on the encoder's name; the `bail!("no hardware H.264
encoder MFT on this machine")` is a message rather than a filter.

That means HEVC needs no new selection logic and no new enumeration: fix the output type and the
same loop finds the NVIDIA HEVC MFT on its own. It was designed that way by accident — the probe
needed to know which encoders existed, and enumerating by input rather than output is what made the
answer complete — and it is worth keeping, because the alternative (matching on "H.264" in the
friendly name) would have quietly pinned the pipeline to one codec.

Worth considering at the same time, though not required: passing `MFVideoFormat_HEVC` as the output
filter would stop the probe from offering H.264-only MFTs as if they were candidates. It changes
only what is reported, since the ones that cannot comply are refused either way.

## Details H.264 let us ignore, which HEVC will not

* **The MP4 tag.** ffmpeg writes HEVC into MP4 as `hev1` by default, which some players refuse;
  `hvc1` is the widely-compatible tag and `-tag:v hvc1` is the usual fix. This has to be checked
  against the replay path and the clips, not assumed.
* **The fragment splitter and the clip `-c copy`** should be unaffected — they work on fragmented
  MP4 boxes, not on bitstream syntax — but "should be" is what gets checked. Same for anything in
  `replay` or the desktop shell that names H.264.
* **The startcode that proves the stream exists.** The H.264 probe asserted the first access unit
  began `[00,00,00,01,09,10,…]`. HEVC has its own NAL header layout, so the probe's assertion needs
  the HEVC equivalent rather than a copy of the H.264 one.

## How to know it works

The same bar as the rest of this work, which is what the user has been promised throughout:

1. The full suite on macOS (544 tests at the time of writing, plus whatever is added).
2. The Windows cross-check across `media`, `replay`, `encoder`, `capture`, `events`.
3. The probe on the box: open the HEVC MFT at 3840x2160, take the ARGB32 texture, produce a stream,
   and confirm the child reads it and the fragment splitter finds the fragments.
4. A 4K soak on the Windows test box with `codec = "hevc"`, reading the same numbers the H.264 hybrid produced
   — CPU, fps, dropped, `capture_copy`, a clean exit, no orphaned ffmpeg — with the default left
   unchanged at H.264 until it has been read. The comparison to beat is **4.0% of one core,
   59.6 fps of 60, 0 dropped, `capture_copy=0.7%`**.

The safety rules for that box do not change: the Windows test box runs Vanguard, so no synthesised input, no
LCU, and no screen capture while a protected game runs. `cargo build`, `cargo test` and the probe
are all safe.
