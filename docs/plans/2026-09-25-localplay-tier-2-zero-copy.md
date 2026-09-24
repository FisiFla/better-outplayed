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

## Sub-steps, each independently verifiable

1. **`Frame` can carry a texture.** `#[cfg(windows)] pub texture: Option<ID3D11Texture2D>` on
   `crates/capture/src/lib.rs::Frame`. There are five construction sites (`wgc.rs`, `stub.rs`,
   `encoder/tests/timeline.rs`, `encoder/src/ffmpeg.rs`, `recorder/src/pump.rs`) and only the
   Windows-compiled ones need the new field. Verified by `cargo check --target
   x86_64-pc-windows-msvc` and by the dev-host suite, in which nothing changes.
2. **The backend can be asked for one.** A capability on `CaptureBackend` (default: no), off by
   default so no behaviour changes until the encoder exists.
3. **`WgcCapture::copy_out` gains the zero-copy branch.** The texture is already extracted from
   the frame's `IDirect3DDxgiInterfaceAccess` (`wgc.rs:439`); the branch returns it with empty
   `data` instead of staging, `CopyResource`, `Map` and memcpy. The frame's reference must be
   held (the pool reuses buffers) — `ID3D11Texture2D` is `Send` in the pinned `windows` 0.58 but
   that says nothing about the pool, so this needs a deliberate look at when the pool recycles.
4. **`guard_frame_size` stops requiring pixels.** It checks `data.len()` today; a texture frame
   has none. It also becomes the place to refuse a texture frame on a path that cannot carry
   one, rather than letting a zero-length frame reach an encoder.
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
