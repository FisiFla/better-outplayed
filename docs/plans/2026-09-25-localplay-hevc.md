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

## Where it stands now: HEVC records, the zero-copy path does not

**HEVC works end to end today on the raw path**, verified on the box with `codec = "hevc"` and the
key left unset:

```
frames=2667  fps=59.6/60  dropped=8  capture_copy=65.4%
buffer session #11 closed: 36 segment(s), 93828980 bytes held in RAM
CLIENT_EXIT=0
codec_name=hevc  codec_tag_string=hvc1  width=3840  height=2160
```

60 fps and 8 dropped frames, against the H.264 raw path's 55.5–56.9 and 8 — the same shape, because
it is the same pipeline with a different encoder. The clip is `hvc1`, so it opens where `hev1` does
not.

**The zero-copy path is blocked, and not by anything in this repository.** The `NVIDIA HEVC Encoder
MFT` is enumerated, activates, and refuses the HEVC output type at 4K with `0xC00D6D76` ("the input
type is not supported for D3D device") while accepting the same type at 640x480. Three things were
tried against it and none moved it:

* an NV12 texture instead of ARGB32 — the format-aware candidate order works (the H.264 encoder now
  opens *taking NV12*, straight from the texture's desc), and HEVC refuses identically;
* `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` on the device, since the message names the device — no change;
* and the outside opinion: `ffmpeg -c:v hevc_mf` cannot open an HEVC encoder at **any** size on this
  machine (`-40`, "Function not implemented"), while `h264_mf` encodes 4K with exit 0.

So the MediaFoundation HEVC path is not functional on this box as a whole, whichever wrapper asks.
The remaining avenue is bisecting the output media type (profile, level, bitrate, `HEVC_ES`) on the
chance one combination it likes exists; that is the next thing to try and not a promising one.

## What the bitrate actually buys, since the expectation matters

**At the same `bitrate_kbps`, HEVC is not a smaller file** — both are rate-controlled to the same
target, and the measured clips say so: 88.1 MB for 45 s of 4K HEVC against 101.9 MB for 90 s of H.264
is *more* bits per second, not fewer. What the same bits buy is **quality**.

The smaller RAM ring the user was promised comes from **lowering the bitrate**, which HEVC then makes
acceptable where H.264 would not: `bitrate_kbps` is a config value, so it is a knob rather than a code
change, and the ring holds whatever the encoder puts in it.

## Two bugs the verification found, both by reading output

1. **A software HEVC run produced H.264.** `EncodeConfig::encoder_name` fell back to a hardcoded
   `libx264` whatever `codec` said, so a `vendor = none` HEVC recording would have been an AVC file
   claiming to be HEVC. The fallback is per codec now (`libx265`), and a new test caught it by
   printing the argument list it was asserting about.
2. **The `hvc1` tag was on the wrong path.** It was written into the *bitstream* arguments — the
   zero-copy hybrid's path — and that is precisely the path HEVC does not run on here, because it is
   the path whose hardware encoder refuses. `ffprobe` on a real clip said `codec_tag_string=hev1`.
   Reading the clip caught what reading the code would not. The tag is on both paths now, with tests
   for the raw one and for H.264 *not* being given it.

## The matrix, and the end of this avenue

The last thing worth trying was the output media type — profile, level, bitrate, keyframe spacing,
subtype — one attribute at a time. Eight variants, asked of every hardware encoder on the box:

```
NVIDIA HEVC Encoder MFT :: exactly what the pipeline builds      -> The input type is not supported for D3D device. (0xC00D6D76)
                        :: no profile and no level                -> 0xC00D6D76
                        :: level 5 instead of 5.1                 -> 0xC00D6D76
                        :: level 4.1                              -> 0xC00D6D76
                        :: no average bitrate                     -> 0xC00D6D76
                        :: no keyframe spacing                    -> 0xC00D6D76
                        :: Main_420_10 instead of Main_420_8      -> 0xC00D6D76
                        :: MFVideoFormat_HEVC_ES                  -> Invalid type. (0xC00D36BD)
```

**Every variant this pipeline controls is refused identically**, and the H.264 encoder refuses every
one of them with `Invalid type` because it is an H.264 encoder. So the refusal is invariant under the
media type: it is not an attribute we are setting wrong.

Put beside the other three results, the conclusion is not a guess:

1. an **NV12** input texture instead of ARGB32 — identical refusal;
2. **`D3D11_CREATE_DEVICE_VIDEO_SUPPORT`** on the device, since the message names the device — no change;
3. **ffmpeg's own `hevc_mf`** cannot open an HEVC encoder at *any* size on this machine (`-40`),
   while `h264_mf` encodes 4K with exit 0;
4. and now every output-type variant, refused identically.

**The zero-copy HEVC path is not achievable on this machine.** The `NVIDIA HEVC Encoder MFT` will not
be configured for a D3D11 texture input — at any attribute combination we can offer. The code is in
place and correct: it enumerates the encoders, carries the codec into the encoder thread, builds the
HEVC output type, and on a machine whose HEVC encoder accepts a texture it would run. This box's does
not. The Intel HEVC encoder that might is listed and fails to activate (`0x80004005`), so it is not an
alternative here either.

**What is left, if this is ever wanted badly enough:** ffmpeg's `hevc_nvenc` *does* work on this box
(verified, 4K60, exit 0), and ffmpeg can take D3D11 textures directly through a hardware device —
`-hwaccel d3d11va` with a shared texture, or the CUDA interop path. That is a different architecture
for the same goal: it would replace the Media Foundation encoder with ffmpeg's own, and it is a design
decision rather than a bug fix. It is not something to start without being asked.

## Withdrawn: it is not the hardware. The refusal is state.

The size ladder, run after everything else in the same process:

```
NVIDIA HEVC Encoder MFT :: 640x480   -> ACCEPTED
                        :: 1280x720  -> ACCEPTED
                        :: 1920x1080 -> ACCEPTED
                        :: 2560x1440 -> ACCEPTED
                        :: 3200x1800 -> ACCEPTED
                        :: 3840x2160 -> ACCEPTED      <- the size the pipeline uses
```

**That is the output type this pipeline builds, at 4K, accepted** — minutes after `probe_output_types`
reported `0xC00D6D76` for the same type at the same size, and after the real open path reported it too.
So the earlier conclusion, written down at length above, was wrong: this is not a machine without a
usable HEVC encoder.

Two things it also rules out, now measured rather than assumed:

* **The order is not the problem.** `probe_ordering` tried all four combinations: output-first gets
  `0xC00D6D76`, input-first gets `0xC00D6D60` — `MF_E_TRANSFORM_TYPE_NOT_SET`, the encoder saying "you
  have not given me the output type yet", which is *confirmation* of the rule the pipeline follows
  rather than a challenge to it. The device manager makes no difference either way.
* **`0xC00D6D76` is `MF_E_UNSUPPORTED_D3D_TYPE`**, Media Foundation's "the type you asked for is not one
  this GPU device supports" — and it is the same error, at the same `SetOutputType` call, that OBS users
  hit with NVENC. The text is the system's name for the HRESULT, not the encoder's own words.

**The one difference between the two attempts is the transform's history.** The ladder activates a
*fresh* transform for every size; `probe_output_types` activates one and sets eight types on it. That
is where to look next — and it means the zero-copy HEVC path is probably reachable rather than blocked.

**A second correction, since it was used as evidence:** ffmpeg's own `hevc_mf` failing at every size
does *not* support "the MediaFoundation HEVC path is broken here". `hevc_mf` is a different encoder —
Microsoft's — which on Windows normally requires the "HEVC Video Extensions" package, so its failure
explains itself and says nothing about NVIDIA's MFT.

**And a flaw in the diagnostic worth fixing before it is trusted again:** `probe_available_types`
skips any encoder whose output type is refused, which is exactly the encoder that needed
interrogating — so the one transform whose *own* answer matters produced no offerings at all.

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
