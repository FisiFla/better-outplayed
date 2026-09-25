# Tier 2 — taking the 4K frame out of system memory

Status: **agreed architecture, not yet implemented.** Replacement for the earlier sketch of this
work, which proposed a full native Media Foundation stack; the measurement below changed that.

## Why this is being done, and what the number is

4K60 with `h264_nvenc` costs **88.9% of one core**, which is the one acceptance criterion still
failing (§13 of `docs/verification-status.md`). The rate is not the problem — 56.9/60 declared,
8 frames dropped of 10,525 — and neither is ffmpeg:

```
frames=8788 ... fps=56.7/60 configured=60 capture=97.1% submit=0.1%
```

`capture=97.1%` is the time this process spends inside `WgcCapture::next_frame`, and at
3840x2160 BGRA that is one `CopyResource` into a staging texture plus one `Map` plus one
row-by-row memcpy into a heap `Vec` — **33.2 MB per frame, ~2.1 GB/s at 57 fps**. `submit=0.1%`
says the encoder's queue never fills, so ffmpeg is not blocking the loop and the cost is not
there.

Two corrections to the sketch this replaces, both measured:

* **PCIe is not saturated.** PCIe 4.0 x16 is ~30 GB/s; the frame uses ~2 GB/s of it. The cost is
  CPU memory bandwidth on the *host* side of the transfer, not the bus.
* **A native muxer would buy nothing.** Since ffmpeg costs 0.1% of the loop, reimplementing
  AAC encoding and fragmented-MP4 muxing natively would add a very large amount of code to
  remove something invisible. It would also mean rebuilding what the replay path rests on:
  fragmented MP4 that `MemoryRingBuffer` parses, the segment-muxer mode a session uses, and the
  `-c copy` clip path.

## The architecture: the MFT does the video, ffmpeg keeps the rest

```
WGC capture ──► ID3D11Texture2D (VRAM)
                    │
                    ├── zero-copy ──► Windows Media Foundation H.264 encoder MFT
                    │                 (IMFDXGIDeviceManager + MFCreateDXGISurfaceBuffer)
                    │                        │ H.264 elementary stream (~2.5 MB/s)
                    │                        ▼
                    └── FIFO ─────────────► ffmpeg  -c:v copy
                                                     + WASAPI audio → AAC
                                                     + segment muxer / fragmented MP4
```

The readback and its two cop~ies disappear; what crosses the pipe becomes ~20 Mbit/s of encoded
video instead of ~2 GB/s of raw pixels, a factor of ~800. Everything downstream of ffmpeg —
`MemoryRingBuffer`, both `EncodeOutput` modes, `ClipSplicer`, the lossless trim — is untouched,
because ffmpeg still writes the containers they consume.

## Step 5's premise, tested before any of step 5 was written (2026-09-25)

The architecture has one load-bearing assumption: that a hardware encoder MFT exists on this
machine **and** will accept a D3D11 device manager over a device of the kind Windows Graphics
Capture requires. If that is false, nothing else in this plan matters.

So it was asked, with `cargo run --release -p localplay-encoder --example mft_probe` — a real
binary that starts Media Foundation, enumerates, and runs the handshake. **No window, no capture
session, no synthesised input**, which matters on a machine running an anti-cheat. The first
answer, before the unlock below, was:

```
hardware H.264 encoder MFTs: 3
  NVIDIA H.264 Encoder MFT                    d3d11=false async=true
      refused: refused MFT_MESSAGE_SET_D3D_MANAGER: The caller does not appear to support
               this transform's asynchronous capabilities. (0xC00D6D77)
  Intel® Quick Sync Video H.264 Encoder MFT   d3d11=false async=true
      refused: could not be activated: Unspecified error (0x80004005)
  Intel® Quick Sync Video H.264 Encoder MFT   d3d11=false async=true
      refused: could not be activated: Unspecified error (0x80004005)
```

Read naively that says "no hardware encoder here takes a GPU texture", which would have killed the
design. It says nothing of the sort. `0xC00D6D77` is `MF_E_TRANSFORM_ASYNC_LOCKED`: an
**asynchronous** MFT refuses to be configured until the caller sets `MF_TRANSFORM_ASYNC_UNLOCK` on
its own attributes — a step every hardware encoder on this box needs, since all three report
`MF_TRANSFORM_ASYNC`. The Intel entries are a separate, honest fact: their MFTs cannot be
instantiated here at all (no usable Intel adapter), so Quick Sync is not a path on this machine.

With the unlock in place, the same probe:

```
hardware H.264 encoder MFTs: 3
  NVIDIA H.264 Encoder MFT                    d3d11=true  async=true
  Intel® Quick Sync Video H.264 Encoder MFT   d3d11=false async=true   (cannot activate)
  Intel® Quick Sync Video H.264 Encoder MFT   d3d11=false async=true   (cannot activate)

of those, 1 accepted MFT_MESSAGE_SET_D3D_MANAGER over a BGRA-capable D3D11 device
```

**NVIDIA's MFT takes the handshake.** The hybrid is viable on this hardware, and that is now
measured rather than assumed — which is what makes the remaining steps worth writing. It also
tells step 5 two things it would otherwise have learned by debugging: the MFT must be unlocked
before it is configured, and the encoder has to be *chosen* rather than assumed, because the
hardware flag enumerates MFTs that cannot be instantiated on this machine at all.

### The chain is now settled: WGC's texture goes straight in

Asking the *right way* answered it. **The NVIDIA MFT accepts `ARGB32`, which is the format Windows
Graphics Capture already delivers** — `DXGI_FORMAT_B8G8R8A8_UNORM` is BGRA bytes with alpha, and
`MFVideoFormat_ARGB32` is Media Foundation's name for exactly that:

```
NVIDIA H.264 Encoder MFT    d3d11=true  async=true
    takes: ARGB32
    takes: NV12
```

So there is **no colour conversion anywhere** and no Video Processor MFT in the chain. Setting the
output type before the input is what made the difference; `NV12` being accepted as well is a bonus
(the MFT could take a converted frame if some future path needed one), but the path this design
wants is the direct one.

**`RGB32` was refused and `ARGB32` accepted**, which is worth knowing precisely because the two look
interchangeable: `RGB32` is BGRX — the alpha byte ignored — and `ARGB32` carries it. Capture
delivers the alpha-bearing variant, so the encoder's preference and the capture's format agree on
the nose.

What step 5 still has to confirm rather than assume: this was measured at **640x480**, because the
question was about the *format*. Whether the same MFT takes `ARGB32` at 3840x2160 is a property of
the encoder's level and of the driver, and it is the first thing step 5 should check — with the
output type set first, since that ordering has now bitten three times in a row.

### What the encoder will *eat* was open, and the reason is instructive

The same probe now also asks each encoder which input subtypes it offers — the question that
decides the chain's shape, because Windows Graphics Capture delivers **BGRA8** and a hardware
encoder that wants **NV12** needs a colour conversion, which belongs on the GPU if it cannot be
avoided. The answer was not the expected one:

```
NVIDIA H.264 Encoder MFT    d3d11=true  async=true
    input types: (could not be asked)
any encoder takes BGRA/ARGB directly: false
video processor MFTs (for a GPU-side colour conversion): none
```

`IMFTransform::GetInputAvailableType` refuses on this MFT even after it has accepted the D3D11
manager, which is consistent with what an **asynchronous** MFT is: it does not offer types for
inspection the way a synchronous one does, and Media Foundation's own guidance for hardware
encoders is to read the *registered* types out of band (`MFTGetInfo`, by the CLSID the activation
carries in `MFT_TRANSFORM_CLSID_Attribute`) rather than to ask an instance. So the honest reading of
that line is "not asked properly yet", **not** "NV12 only" — and the `false` beside it therefore
means nothing yet either.

Two things follow, both for step 5:

1. **Settle the input format by trying it, not by enumerating it.** `SetInputType` with a candidate
   `IMFMediaType` (BGRA/RGB32 first, then NV12) answers the question directly: an MFT accepting a
   type is the fact that matters, and it is the same call the encoder will make anyway. `MFTGetInfo`
   by CLSID is the alternative.
2. **If the conversion is needed, the chain needs a processor and this probe did not find one**
   under the hardware flag. That flag is the wrong filter for a Video Processor MFT, which is
   normally a software MFT that drives the GPU's video processor — so the next probe should look at
   the whole category, and should test BGRA→NV12 negotiation rather than mere presence.

Neither is a reason to doubt the design; the D3D11 handshake above is the part that could have
killed it, and it passed. It is a reason not to write step 5's type negotiation from the assumption
that a captured texture can go straight in.

### The 4K encoder opens with ARGB32 — the unknown at the top of this section, answered

`MftEncoder::open(&device, (3840, 2160))` now succeeds and selects the encoder rather than assuming
one:

```
--- the encoder core, at 3840x2160 ---
opened NVIDIA H.264 Encoder MFT for (3840, 2160), taking ARGB32 input
push_texture failed: IMFTransform::ProcessInput: The callee is currently not accepting further
  input. (0xC00D36B5)
```

So the chain is what the design wanted: the capture path's own D3D11 device, the capture's own
format, at full resolution. Three things had to be right at once, and the two that were wrong are
worth recording because each looked like something else:

* **Two frame rates.** The output type said 30 fps and the input type said 60 — in two functions,
  from two literals. Media Foundation refused with `MF_E_INVALIDMEDIATYPE`, whose own text is
  *"invalid, inconsistent, or not supported"*, and the word doing the work was "inconsistent":
  nothing was wrong with either rate alone. The fix is structural rather than a corrected number —
  both types now take their rate from one `FRAME_RATE` constant, so they cannot disagree.
* **No profile, no level.** With neither, this MFT refuses *every* input format at 3840x2160,
  including NV12 — which it encodes for ffmpeg every day. A level is a claim about what the stream
  can carry, and 4K at 60 needs 5.2 where 4K at 30 fits 5.1, which is why `FRAME_RATE` is 30 and the
  type declares `eAVEncH264VLevel5_1` and `eAVEncH264VProfile_High`.

Both were fixed together, so which of them the refusal turned on is not isolated — the rate
inconsistency was certainly a real defect, and the profile and level are certainly load-bearing at
4K. What *is* isolated is the answer: **the 4K input format is ARGB32, and the chain needs no
conversion.**

**`ProcessInput` returning `MF_E_NOTACCEPTING` is not a failure — it is the async contract.** An
asynchronous MFT takes input only when it has said it wants some, signalled as
`METransformNeedInput` through the event generator. So the next piece of step 5 is the standard
async loop, and it is now known work rather than an open question:

1. `MFT_MESSAGE_NOTIFY_BEGIN_STREAMING` before the first frame.
2. Drain `GetEvent` and answer the messages: `METransformNeedInput` → push the next captured
   texture, `METransformHaveOutput` → `ProcessOutput` into a sample and take the H.264 bytes.
3. `MFT_MESSAGE_COMMAND_DRAIN` at the end, then keep draining until the output stops.

## Sub-steps, each independently verifiable

1. **`Frame` can carry a texture.** ✅ **Done.** `pub texture: Option<GpuTexture>` on
   `crates/capture/src/lib.rs::Frame`, plus `Frame::has_pixels()`. Five construction sites took
   `texture: None`. Verified by `cargo check --target x86_64-pc-windows-msvc` (clean) and the
   dev-host suite (537 passed).
   Two things this step found that the sketch did not know, both worth carrying forward:

   * **A bare `ID3D11Texture2D` field would have stripped three derives off `Frame` on every
     platform.** The `windows` crate's COM interfaces implement neither `Clone`, `PartialEq` nor
     `Debug`, and `Frame` derives all three — so the field is a newtype, `GpuTexture`, whose three
     impls are the interface's own semantics written out once: `Clone` is an `AddRef` (this is the
     frame-pool lifetime question, in one place), `PartialEq` is COM identity, `Debug` prints the
     pointer. Off Windows the type is **uninhabited**, so `texture` there can only be `None`.
   * **`cast()` and `as_raw()` live on the `Interface` trait**, which has to be in scope. The
     cross-check caught this; nothing else could have, since the code compiles only for Windows.
   * (The sketch's step 4 was wrong about `guard_frame_size`: it checks only width and height today,
     never `data.len()`, so it needed no change. The guard that *is* needed went into
     `FfmpegEncoder::submit_video` instead — see step 4 below.)
2. **The backend can be asked for one.** A capability on `CaptureBackend` (default: no), off by
   default so no behaviour changes until the encoder exists.
3. **`WgcCapture::copy_out` gains the zero-copy branch.** The texture is already extracted from
   the frame's `IDirect3DDxgiInterfaceAccess` (`wgc.rs:439`); the branch returns it with empty
   `data` instead of staging, `CopyResource`, `Map` and memcpy. The frame's reference must be
   held (the pool reuses buffers) — `ID3D11Texture2D` is `Send` in the pinned `windows` 0.58 but
   that says nothing about the pool, so this needs a deliberate look at when the pool recycles.
4. **A frame with no pixels is refused by the encoder that cannot carry one.** Done, in
   `FfmpegEncoder::submit_video`: it writes `data` to the child's stdin, so a texture frame would
   become a zero-length rawvideo frame that ffmpeg reads as the next frame's leading bytes — a
   desync surfacing as corruption seconds later, pointing at nothing. Refused with the reason,
   and asserted by `a_frame_without_pixels_is_refused_rather_than_written_as_nothing`. This is
   what makes step 1 safe to land *before* the encoder that consumes textures exists.
5. **`WmfEncoder`** (`crates/encoder/src/wmf.rs`, `#[cfg(windows)]`): `MFStartup`,
   `MFCreateDXGIDeviceManager` + `ResetDevice`, the encoder MFT found by **`MFTEnumEx`** — the
   pinned `windows` 0.58 does **not** expose `CLSID_CMSH264EncoderMFT`, and
   `IMFDXGIDeviceManager` lives in `Win32::System::WinRT::Media`, not beside the other MF
   functions — `MFT_MESSAGE_SET_D3D_MANAGER`, input type at the capture size,
   `CODECAPI_AVLowLatencyMode`, and `MFCreateDXGISurfaceBuffer` per frame. Its output is the
   H.264 elementary stream that step 6 consumes.
6. **ffmpeg takes the encoded stream.** `-c:v copy` with the MFT's output on a pipe, audio and
   muxing unchanged. The one real risk lives here: `-force_key_frames` cannot apply to a copied
   stream, so **the GOP is the MFT's to set** (`CODECAPI_AVEncMPVGOPSize`), and the segment
   muxer must cut at those keyframes for the one-fragment-per-keyframe contract that
   `MemoryRingBuffer` and the clip path depend on. This wants a test before it wants trust.
7. **Selection and fallback.** `WmfEncoder` only when Windows and no software encoder was
   requested; `FfmpegEncoder` everywhere else, including every test.
8. **Re-soak.** The same 180 s 4K/60 soak that produced 88.9%, expecting `capture≈0%` and the
   process under 3% of one core, with `dropped=` still ~0 and the timeline (`media=` vs `wall=`)
   unchanged from §13.

## Known constraints to carry

* `windows` 0.58: `MFCreateDXGISurfaceBuffer`, `MFCreateDXGIDeviceManager`, `MFCreateSample`,
  `MFCreateMediaType` are present; `CLSID_CMSH264EncoderMFT` is not.
* The D3D11 device needs `ID3D11Multithread` protection once capture and the MFT touch it from
  different threads; WGC writes to it and the MFT reads from it.
* The Windows cross-check covers `media`, `replay`, `encoder`, `capture`, `events`, `xtask` and
  **not** `store`, `recorder`, `cli`, `desktop` (rusqlite's bundled C), so a mistake in the
  recorder's Windows path is caught by the box build and not by the cross-check.
* Suite counts are **535** Rust workspace, **112** desktop shell, **134** vitest — not 567.
