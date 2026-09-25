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

## The thing that was wrong, written down plainly

The first version of this work fixed the codec everywhere it was *named* and still failed on the
box, and the reason is worth keeping because I had confidently written the opposite here first.

> `MFTEnumEx`'s third argument is `pInputType` and its fourth is `pOutputType`.

The enumeration passed `None` for the third and `MFVideoFormat_H264` for the fourth, and read it as
"encoders that consume video and produce H.264" — which is right, and is a *filter on the output
subtype*. So the phrase "nothing matches on a name, and the same loop will find the HEVC MFT
automatically" was wrong: the loop could never return an HEVC encoder at all, because the question
asked only for H.264 encoders. `configure_encoder`'s negotiation was never reached for it.

What that looked like on the box, against a machine that very much can do HEVC:

```
--- asking about Hevc ---
hardware Hevc encoder MFTs: 3          <- the same three MFTs, all named "...H.264 Encoder MFT"
  refused: SetOutputType(Hevc): Invalid type. (0xC00D36BD)
```

And the same machine, from ffmpeg:

```
hevc_nvenc   NVIDIA NVENC hevc encoder (codec hevc)
hevc_mf      HEVC via MediaFoundation (codec hevc)
$ ffmpeg -f lavfi -i testsrc=3840x2160:rate=60 -t 2 -c:v hevc_nvenc -f null -   -> exit 0
```

So the hardware was never in question; the question was. The fix is one function —
`output_subtype(codec)`, used by both the enumeration filter and the output media type — and the
lesson is the same one this project keeps paying for and keeps writing down: a probe that answers
the wrong question answers it convincingly.

## Where HEVC actually stands: the encoder is found, and it wants NV12

Fixed and verified on the box. Both enumerations had a type filter pinned to `MFVideoFormat_H264`
as `pOutputType` — `probe_started_encoders` and, the one that matters, `open_started`, which decides
what a recording uses. `MFT_ENUM_FLAG_SORTANDFILTER` went with them: it filters against a *preference*
list, which has nothing to say without a type to prefer for. Measured, with the filter:

```
--- asking about Hevc ---
hardware Hevc encoder MFTs: 3        <- the two H.264 encoders and one Quick Sync encoder
```

and without it, the machine's real inventory:

```
--- every hardware video encoder MFT here, unfiltered: 10 ---
  NVIDIA HEVC Encoder MFT              <- the one this task needs
  NVIDIA H.264 Encoder MFT
  Intel® Hardware H265 Encoder MFT
  ... VP9, AV1, Quick Sync
```

The enumeration's job is now to be *complete* and the asking to be precise — hand it every hardware
encoder and let each say for itself what it will do, which is what this code already did on the input
side and explains at length, because the input side learned this lesson first.

**And the answer it gives for HEVC is a real one, not another filter artefact:**

```
NVIDIA HEVC Encoder MFT: SetOutputType(Hevc): The input type is not supported for D3D device. (0xC00D6D76)
```

That MFT is found, it is activated, and it **accepts the HEVC output type** — where the H.264 one
answers `Invalid type (0xC00D36BD)`. It then refuses the *input*: ARGB32 in a D3D11 texture is not
something it will take, which is what the error says in as many words. HEVC encoders work in NV12 or
P010; the H.264 MFT is the unusually accommodating one at this size.

**So the next piece of work is an ARGB32 → NV12 conversion on the GPU**, in the capture handover,
before the texture reaches the encoder — `ID3D11VideoProcessor` with a `VideoProcessorBlt`, which is
a GPU-side pass and keeps the property that matters: no pixels cross to the CPU. The handover already
blits with `CopyResource`, which cannot change format, so this replaces that blit rather than adding
one. Everything downstream of the encoder is unchanged and already verified — `-f hevc`, the `hvc1`
tag, the fragment splitter, the clip `-c copy`.

**Worth stating plainly because it is easy to lose:** the *raw* path can already do HEVC today, with
`codec = "hevc"` and ffmpeg's `hevc_nvenc` — verified on the box, 2 s of 4K60, exit 0. It just costs
the readback, which is the 88.9% of a core this whole line of work exists to delete. So HEVC is
available now as a trade, and available *without* the trade once the NV12 pass exists.

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
