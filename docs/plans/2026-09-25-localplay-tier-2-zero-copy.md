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

### H.264 out of the 4K encoder, from GPU textures, with no CPU copy

The async loop closes the video half, and it is measured end to end on the box:

```
--- the encoder core, at 3840x2160 ---
opened NVIDIA H.264 Encoder MFT for (3840, 2160), taking ARGB32 input
30 frames submitted, 9342 bytes of H.264 out
first output chunk: 2250 bytes, starts [00, 00, 00, 01, 09, 10, 00, 00]
```

Thirty textures went in and a real elementary stream came out: `00 00 00 01` is an Annex-B start
code and `09` is an access-unit delimiter. **No pixel of those frames crossed the CPU** — that is
the entire point of the exercise, and it now works at 3840x2160 with the capture path's own format
on the capture path's own device.

The bytes are small (9.3 KB for 30 frames) because the frame is zero-filled and compresses to
nothing; the size is not the finding, the format is.

What the async contract cost, and why it is standard rather than mysterious:

* `MFT_MESSAGE_NOTIFY_BEGIN_STREAMING` and `NOTIFY_START_OF_STREAM` before the first frame — an
  async MFT will not take input before it has been told streaming began;
* `GetEvent(MF_EVENT_FLAG_NO_WAIT)` in a poll, answering `METransformNeedInput` by pushing the next
  texture and `METransformHaveOutput` by calling `ProcessOutput`;
* `MF_E_TRANSFORM_NEED_MORE_INPUT` from `ProcessOutput` is not an error — it is "nothing yet";
* `MFT_MESSAGE_COMMAND_DRAIN` at the end, then keep draining, or the last frames stay inside the
  encoder and never reach the container.

**What is left is integration, not discovery.** The video half is proven; the remaining work is
handing these bytes to ffmpeg (step 6) and re-soaking (§13's 88.9% is the number to beat). The
open risk there is unchanged and named: `-force_key_frames` cannot apply to a copied stream, so the
GOP has to come from the MFT and the segment muxer has to cut at those keyframes for the
one-fragment-per-keyframe contract that `MemoryRingBuffer` and the clip path depend on.

### The whole hybrid, end to end, on the box

Everything the design needs is now measured in one run, with no capture session and no input:

```
opened NVIDIA H.264 Encoder MFT for (3840, 2160), taking ARGB32 input
90 frames submitted, 28028 bytes of H.264 out
the stream starts [00, 00, 00, 01, 09, 10, 00, 00]
ffmpeg produced 29711 bytes of fragmented MP4
the project's own splitter sees 3 fragment(s):
  [(0, 0, 997, true), (1, 997, 999, true), (2, 1997, 967, true)]
```

Read that against the plan's risk list, which is now empty:

| Risk | Outcome |
|---|---|
| Does a hardware encoder MFT take a D3D11 device manager? | **yes**, once it is async-unlocked |
| Does it take the capture's own format at 4K? | **yes** — `ARGB32` |
| Does the async contract work? | **yes** — the event loop, 90 frames through it |
| Will ffmpeg take the bitstream with `-c:v copy`? | **yes** — and the project's own `FragmentSplitter` parses what comes out |
| Do the timestamps survive? | **yes** — 965 ms of container for 990 ms of paced frames, streamed |
| Can the GOP be imposed without `-force_key_frames`? | **yes** — `MF_MT_MAX_KEYFRAME_SPACING`, and the three fragments above are 1 s apart and all keyframes |

That last row is the one the plan called the place this would hurt, and the fragments are the
answer: one per second, each a keyframe, which is the contract `MemoryRingBuffer` parses and the
lossless clip path cuts on.

### One defect of the probe's own, kept because it is a design requirement

The first version of this test collected the whole bitstream and handed it to ffmpeg in one write.
The result was **196 ms of container for a second of paced frames** — because arrival timestamps
only mean anything if the bytes arrive when they were made, and a batch write makes every frame
arrive at once. Streaming each access unit as the encoder produces it gives 965 ms for 990 ms.

So the requirement is not "hand ffmpeg the bitstream" but **"stream it as it is produced"**, and
that is a property of the pipeline rather than of the test: the recorder's own writer will have to
do it. Recorded here because a future version that buffers "for efficiency" would silently halve
the frame rate of every recording it made.

### What is left

**Integration, and then the number.** `MftEncoder` and its ffmpeg sink are written and proven in
the probe; what remains is wiring them into the recorder behind `cfg(windows)` — capture handing
over its texture, the pump not reading back — and then the re-soak against §13's
**88.9% of one core**, which is the number all of this exists to move.

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
2. **The backend can be asked for one.** ✅ **Done.** `CaptureBackend::deliver_textures` (default:
   a no-op, so a backend that cannot do it keeps delivering pixels and every consumer is unaffected),
   overridden by `WgcCapture`. The answer is remembered on the backend as well as the session, so it
   survives a `stop`/`start` cycle and can be given *before* `start` — which is when an encoder is
   configured and therefore when it decides whether it can take a texture.
3. **`WgcCapture::copy_out` gains the handover branch.** ✅ **Done**, and the lifetime question the
   sketch flagged changed what this is called. **WGC recycles the surface it hands out**, so the
   frame's own texture cannot be passed on at all — it is valid only until `Close()`. What goes to
   the encoder is therefore a texture *we* own, with the captured surface blitted into it on the GPU.
   So this is not zero-copy; it is **no CPU copy**, which is the thing that costs 97% of the loop: one
   `CopyResource` replaces a 33 MB copy through system memory, per frame.
   The blit needs a **ring**, not one texture: a single one would be overwritten by the next frame
   while the encoder was still reading it. Three deep — the standard slack for a hardware encoder
   keeping one or two frames in flight; deeper would only cost VRAM at 33 MB a texture. The ring is
   built on first use, so a session never asked for textures pays nothing.
4. **A frame with no pixels is refused by the encoder that cannot carry one.** Done, in
   `FfmpegEncoder::submit_video`: it writes `data` to the child's stdin, so a texture frame would
   become a zero-length rawvideo frame that ffmpeg reads as the next frame's leading bytes — a
   desync surfacing as corruption seconds later, pointing at nothing. Refused with the reason,
   and asserted by `a_frame_without_pixels_is_refused_rather_than_written_as_nothing`. This is
   what makes step 1 safe to land *before* the encoder that consumes textures exists.
5. **The hardware encoder.** ✅ **Done**, as `MftEncoder` in `crates/encoder/src/mft.rs` rather
   than a `wmf.rs` — the name in the sketch presumed an `Encoder` implementation, and what was
   needed first was the core it will be built on, proven on its own. It selects an encoder by
   `MFTEnumEx` (the pinned `windows` 0.58 does not expose `CLSID_CMSH264EncoderMFT`; it and
   `IMFDXGIDeviceManager` are both in `Media::MediaFoundation`, not `System::WinRT::Media` as the
   sketch expected), unlocks it for asynchronous use, hands it the capture path's **own** device
   manager, sets its output type — profile, level, bitrate and **the GOP**
   (`MF_MT_MAX_KEYFRAME_SPACING`) — before negotiating the input, and drives the async event loop.
   Proven on the box: 90 frames of 4K `ARGB32` in, an H.264 elementary stream out, no CPU copy.
6. **ffmpeg takes the encoded stream.** ✅ **The shape is proven, not yet wired.** `-c:v copy` with
   the MFT's output on a pipe, audio and muxing unchanged, and the risk the sketch named is closed:
   the GOP comes from the MFT, and ffmpeg produced **three fragments a second apart, each a
   keyframe**, which the project's own `FragmentSplitter` read back. Two things that only came from
   doing it: the bitstream must be **streamed as it is produced** (a batch write made ffmpeg stamp
   every frame at once — 196 ms of container for a second of frames, against 965 ms when streamed),
   and the 4K input type is `ARGB32`, tested rather than enumerated, because this MFT will not
   enumerate its types and refuses `ARGB32` at some sizes and not others.
7. **Selection and fallback.** The hardware path only when Windows and no software encoder was
   requested; `FfmpegEncoder` everywhere else, including every test. **Still to write**, and it is
   the same seam as step 6's wiring: an `Encoder` implementation that drives `MftEncoder` and its
   ffmpeg child, reusing the existing audio input and output arguments — the hybrid changes the video
   input and the video codec and nothing else.
8. **Re-soak.** The same 180 s 4K/60 soak that produced 88.9%, expecting `capture≈0%` and the
   process under 3% of one core, with `dropped=` still ~0 and the timeline (`media=` vs `wall=`)
   unchanged from §13.

### The encoder takes its device from the frame, not from a capability

Step 7's wiring needs the MFT opened on the **capture path's** device — an MFT can only be handed a
texture from the device its manager was built over. The obvious design is a capability on
`CaptureBackend` that hands the device out; it was not taken, because there is a stronger source:

```rust
let device = texture.GetDevice()?;              // whatever made this frame
let size = texture.GetDesc().Width/Height;      // whatever shape it will be
MftEncoder::open(&device, size)
```

`MftEncoder::open_for_texture` asks the frame. That removes the question rather than answering it:
a handoff capability would have had two components agreeing about which device capture uses, and the
failure mode of their disagreement is a texture the encoder silently cannot see. A texture's device
is its own by definition.

It also removes an API: no `CaptureBackend::d3d11_device`, no newtype to carry an interface between
two crates, and nothing to keep in step when capture's device handling changes.

### The first end-to-end soak with the hybrid: the whole path runs, and it is slower

`encode.zero_copy = true`, the same 180 s 4K/60 soak that produced 88.9% of one core, the shipping
`vendor = "auto"` resolving to `h264_nvenc`. Everything the hybrid needs happened:

```
zero-copy: captured textures go straight to the hardware encoder, and ffmpeg copies the H.264
the hardware encoder is open encoder=NVIDIA H.264 Encoder MFT format=ARGB32
```

— the MFT opened on the capture path's own device at 3840x2160, frames went through it, and
fragments came out the far end. **And the pipeline is measurably worse for it:**

| | raw path (the 88.9% soak) | hybrid |
|---|---|---|
| `fps=` | 56.9/60 | **34.9/60** |
| `dropped=` | 8 | **3787, still climbing** |
| `capture=` | 97.1% | 94.5% |
| `submit=` | 0.1% | 1.0% |

**The diagnosis is the design's, and it is specific: `submit_video` blocks the pump.** A textured
frame is encoded *inside the pump's call* (`FfmpegEncoder::encode_texture`), and that call waits up
to two seconds for the asynchronous MFT to ask for input. So the pipeline is no longer paced by
`FramePacer` — it is paced by whatever rate the MFT happens to consume at, frames pile up behind it,
and the encoder's bounded queue drops them. That is exactly what `dropped=3787` is, and why the
achieved rate fell to 58% of the raw path's.

The fix is the shape the ffmpeg side already has, one level earlier: **a queue and a thread for the
MFT**, so the pump hands over a texture and returns. Two things make that less trivial than it
sounds and both are worth stating before it is written:

* **The texture lifetime gets tighter.** The handover ring is three deep, so a texture is rewritten
  three frames after it is handed out. A queue in front of the MFT has to be shallower than the ring,
  or the encoder will be reading a texture that capture has already reused. Three and two is the
  obvious arrangement and it needs to be reasoned about rather than picked.
* **The wait must not become a stall.** A per-frame wait of up to two seconds is a wedge, not a
  timeout; the pump's own deadline is 5 ms. Whatever replaces this needs its blocking confined to the
  thread that owns the MFT.

**What is not claimed:** the CPU figure for the hybrid. The measurement landed on the *splicing*
phase at the end of the soak (0.1%, the parent idle while ffmpeg copies) and is worthless; and with
the achieved rate wrong, any CPU comparison is confounded anyway — fewer frames per second is less
work per second. The number that matters can only be taken once the pump is not throttled by the
encoder, and until then this design has not been shown to reduce CPU at all.

## Known constraints to carry

* `windows` 0.58: `MFCreateDXGISurfaceBuffer`, `MFCreateDXGIDeviceManager`, `MFCreateSample`,
  `MFCreateMediaType` are present; `CLSID_CMSH264EncoderMFT` is not.
* The D3D11 device needs `ID3D11Multithread` protection once capture and the MFT touch it from
  different threads; WGC writes to it and the MFT reads from it.
* The Windows cross-check covers `media`, `replay`, `encoder`, `capture`, `events`, `xtask` and
  **not** `store`, `recorder`, `cli`, `desktop` (rusqlite's bundled C), so a mistake in the
  recorder's Windows path is caught by the box build and not by the cross-check.
* Suite counts are **535** Rust workspace, **112** desktop shell, **134** vitest — not 567.
