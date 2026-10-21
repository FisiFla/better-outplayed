//! Windows Graphics Capture (WGC) video backend.
//!
//! WGC hands over one D3D11 texture per captured frame; this backend copies it into system
//! memory as BGRA8, the known cost of the ffmpeg-sidecar encode path. The copy is paid only for
//! frames the caller keeps: the surplus a display delivers above the configured rate (53-75fps
//! against 30 measured) is closed by [`CaptureBackend::discard_pending`] with no staging texture,
//! `CopyResource`, `Map` or allocation (see [`Session::discard_pending`]).
//!
//! It type-checks for `x86_64-pc-windows-msvc` from macOS (`cargo check` does not link) and has
//! been run on a 4K/150%-scaled Windows 11 desktop with a full-screen game up, producing real
//! content in every segment. `copy_out` must `CopyResource` the captured texture into the
//! staging texture before the `Map`, or every mapped frame is zero-filled (pure black); see its
//! SAFETY comment.
//!
//! # Frame arrival
//!
//! `CaptureBackend::next_frame` is a pull API, so this backend waits on the frame pool rather
//! than polling it. The pool is created free-threaded, which removes the need for a dispatcher
//! queue and makes the pool raise `FrameArrived` on its own worker thread; the handler behind
//! that event does nothing but set a flag and wake the condvar the wait is on. The wait therefore
//! ends when the pool says a frame is there, not a millisecond later.
//!
//! # Known gaps
//!
//! - A display mode change mid-capture normally needs `Direct3D11CaptureFramePool::Recreate`,
//!   which this backend does not call. It copies whatever size the incoming texture is
//!   (re-creating its staging texture when that changes) while [`WgcCapture::size`] keeps
//!   reporting the capture item's size. If the two diverge the CLI refuses the frame and names
//!   both sizes rather than feeding ffmpeg a differently sized frame.
//! - Monitor and window selection (`[capture]`) has never run on Windows; it is type-checked
//!   only. A captured window resized mid-capture hits the same size-divergence gap, and a
//!   minimised one stops delivering frames.
//! - Every frame the caller keeps is copied through a staging texture, the cost accepted to keep
//!   the encoder an ffmpeg sidecar; discarded frames skip it.

#![cfg(windows)]

use crate::platform::init_com;
use crate::target::{ordered_monitors, pick_monitor, pick_window, CaptureTarget};
use crate::{clock_base, take_or_wait, CaptureBackend, Frame, FrameSignal, PixelFormat};
use anyhow::{bail, Context, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::BOOL;
use windows::core::{factory, IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureAccess,
    GraphicsCaptureAccessKind, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::{IDirect3DDevice, IDirect3DSurface};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::DisplayId;
use windows::Graphics::SizeInt32;
use windows::Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus;
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource,
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, MonitorFromWindow, HDC, HMONITOR, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::Com::CoUninitialize;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
};

/// Buffers in the frame pool. A consumer that copies each frame out before asking for the next needs only a couple.
const BUFFER_COUNT: i32 = 2;

/// Upper bound on the frames one `discard_pending` call closes.
///
/// A well-behaved pool holds only `BUFFER_COUNT`; the bound stops a pool that keeps answering from
/// making the drain unbounded. Hitting it is not an error: every frame taken is closed and the
/// next call takes the rest.
const DISCARD_LIMIT: usize = BUFFER_COUNT as usize * 4;

/// Monitor or window capture, per [`CaptureTarget`].
pub struct WgcCapture {
    /// What to capture. Kept so a restart after `stop` rebuilds the same item.
    target: CaptureTarget,
    /// The capture item's size in physical pixels (`GraphicsCaptureItem::Size()`): the size the
    /// frame pool is created with, hence of every texture WGC hands back.
    ///
    /// Not `GetSystemMetrics(SM_CXSCREEN/SM_CYSCREEN)`: that is DPI-virtualised, so a process that
    /// is not per-monitor DPI aware gets logical pixels (2560x1440 on a 4K display at 150% scaling
    /// while the item was 3840x2160). Sizing the rawvideo pipe from it mismatches the frames.
    size: (u32, u32),
    session: Option<Session>,
    /// Whether the caller asked for textures rather than pixels. Held here too so the answer survives
    /// `stop`/`start` and can be given before `start`, when an encoder decides whether it can take one.
    want_textures: bool,
    /// Time spent inside `poll_frame` (the copy), accumulated. See [`replayed_capture::FrameTimings`].
    copying: Duration,
    /// Time blocked waiting for the frame pool to say a frame arrived, accumulated.
    ///
    /// The wait itself, as it happened: a wait overshoots by the scheduler's granularity, and the
    /// overshoot is part of the cost.
    waiting: Duration,
    /// The pool's frame-arrival signal: the flag the `FrameArrived` handler sets and the wait
    /// blocks on.
    ///
    /// Shared with that handler, which the pool owns, so it is an `Arc`. Held here as well as in
    /// the session so the wait can be taken without borrowing the session the poll needs.
    signal: Arc<FrameSignal>,
    /// Whether `start` has been called and `stop` has not.
    ///
    /// Separate from `session`, which is built in [`Self::new`] because the item size must be known
    /// before the encoder is configured, between construction and `start`.
    started: bool,
    /// Whether this thread's COM apartment was taken by us and must be given back.
    com_owned: bool,
}

/// The ring of textures a session hands to an encoder that asked for them.
///
/// Sized on first use and recreated if the geometry changes, as for the staging texture: the
/// capture item's size can change with the display mode.
#[derive(Default)]
struct Handover {
    ring: Vec<ID3D11Texture2D>,
    next: usize,
    size: (u32, u32),
    format: DXGI_FORMAT,
}

impl Handover {
    /// Textures in the ring: a hardware encoder's one or two frames in flight plus the one being copied into.
    const DEPTH: usize = 3;

    /// The next texture in the ring, building it if there is none or the geometry changed.
    fn texture(
        &mut self,
        device: &ID3D11Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> Result<ID3D11Texture2D> {
        if self.ring.is_empty() || self.size != (width, height) || self.format != format {
            let mut ring = Vec::with_capacity(Self::DEPTH);
            for _ in 0..Self::DEPTH {
                ring.push(create_handover_texture(device, width, height, format)?);
            }
            self.ring = ring;
            self.next = 0;
            self.size = (width, height);
            self.format = format;
        }
        let picked = self.next % self.ring.len();
        self.next = picked + 1;
        Ok(self.ring[picked].clone())
    }
}

/// One texture for the handover ring.
///
/// `BIND_RENDER_TARGET | BIND_SHADER_RESOURCE` is the shape WGC's own frames carry and a hardware
/// encoder MFT accepts as `ARGB32` input (measured at 3840x2160 on the Windows test machine).
/// Narrower bind flags would be refused for a reason unrelated to the format.
fn create_handover_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut created: Option<ID3D11Texture2D> = None;
    // SAFETY: `created` is the out-parameter and `desc` outlives the call. No initial data: the
    // first thing that happens to this texture is a copy from a captured frame.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut created)) }
        .with_context(|| format!("creating a handover texture {width}x{height}"))?;
    created.context("CreateTexture2D returned no texture")
}

/// Everything a session acquires, so there is one thing to release.
struct Session {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pool: Direct3D11CaptureFramePool,
    /// The token the pool gave for the `FrameArrived` registration, so it can be taken back.
    ///
    /// The handler behind the registration is a COM object the pool holds a reference to;
    /// removing the registration is what lets the pool let it go.
    arrival: i64,
    /// The capture item's size in physical pixels; the frame pool is created with it, so it is the
    /// size of the frames to come.
    size: (u32, u32),
    /// The textures handed out when textures were asked for, and how many have gone round. `None`
    /// until asked.
    ///
    /// A ring, not one texture. WGC recycles the surface it gives us, so the captured texture cannot
    /// be held past `frame.Close()`; the handed-over texture must be one we own, with the frame
    /// copied into it on the GPU. A single one would race: the next frame would overwrite it while
    /// the encoder still reads it.
    handover: Option<Handover>,
    /// Held deliberately: the pool and session capture this item, and dropping our reference while
    /// they run is not documented as safe. Nothing below `start` reads it.
    _item: GraphicsCaptureItem,
    /// Closed by `stop`; kept for the lifetime of the capture otherwise.
    capture: GraphicsCaptureSession,
    /// Re-created when the frame size or format changes (resolution switch).
    staging: Option<Staging>,
}

type Staging = (u32, u32, ID3D11Texture2D);

impl WgcCapture {
    /// `target` defaults to the primary monitor; an unknown monitor or window is an error, not a
    /// quiet capture of something else.
    pub fn new(target: &CaptureTarget) -> Result<Self> {
        let mut this = Self {
            target: target.clone(),
            size: (0, 0),
            session: None,
            want_textures: false,
            copying: Duration::ZERO,
            waiting: Duration::ZERO,
            signal: Arc::new(FrameSignal::new()),
            started: false,
            com_owned: false,
        };
        this.start_session()?;
        Ok(this)
    }

    /// Take this thread's COM apartment and build the WGC session on it.
    fn start_session(&mut self) -> Result<()> {
        self.com_owned = init_com()?;
        match Session::new(&self.target, Arc::clone(&self.signal)) {
            Ok(mut session) => {
                self.size = session.size;
                // Carry the caller's answer into the session: the encoder is configured, and asks, between
                // construction and `start`, before this session existed.
                session.deliver_textures(self.want_textures);
                tracing::info!(
                    width = self.size.0,
                    height = self.size.1,
                    "the capture item measures these physical pixels"
                );
                self.session = Some(session);
                Ok(())
            }
            Err(e) => {
                // No frame was captured, so give the apartment back rather than leave the thread in one nobody owns.
                self.release_com();
                Err(e)
            }
        }
    }

    /// The capture item's size in physical pixels (see the `size` field).
    ///
    /// The geometry of every frame the backend produces, so the rawvideo pipe must be declared with
    /// it. A WGC frame carries its own size, which can differ after a resolution or scaling change;
    /// the CLI refuses such a frame.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    fn release_com(&mut self) {
        if self.com_owned {
            // SAFETY: `start_session` took the apartment on this thread.
            unsafe { CoUninitialize() };
            self.com_owned = false;
        }
    }
}

impl CaptureBackend for WgcCapture {
    fn start(&mut self) -> Result<()> {
        if self.started {
            bail!("WGC capture is already started");
        }
        // `new` already built the session; this covers a restart after `stop`, which tears it down and
        // gives the COM apartment back.
        if self.session.is_none() {
            self.start_session()?;
        }
        self.started = true;
        Ok(())
    }

    /// Waits for the next frame, splitting its time into copying and waiting. Returns `Ok(None)`
    /// when no frame arrives within `timeout`, per the `CaptureBackend` contract.
    ///
    /// Each is timed where it happens rather than by timing the whole call and subtracting: the
    /// copy is the handover of a frame the pool has, and everything else is the wait for one to
    /// arrive. The wait's own clock reads (nanoseconds) are left out of both. See
    /// [`crate::FrameTimings`].
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        if !self.started {
            bail!("WGC capture is not started");
        }
        let session = self
            .session
            .as_mut()
            .context("WGC capture has no session (already stopped)")?;
        let mut copied = Duration::ZERO;
        // The pool is asked for a frame first, because a frame it already holds costs nothing to
        // take; when it has none, the wait blocks until the pool's own `FrameArrived` handler
        // says one is there. Nothing polls in between.
        let (frame, waited) = take_or_wait(
            &mut || {
                let before = Instant::now();
                let polled = session.poll_frame();
                copied += before.elapsed();
                polled
            },
            &self.signal,
            timeout,
        )?;
        self.copying += copied;
        self.waiting += waited;
        Ok(frame)
    }

    fn stop(&mut self) -> Result<()> {
        self.started = false;
        if let Some(session) = self.session.take() {
            // Close the session before the pool, the order every WGC sample uses, and report a refusal from
            // either rather than pretending it stopped.
            if let Err(e) = session.capture.Close() {
                tracing::warn!("closing the WGC capture session failed: {e}");
            }
            // Take the arrival handler back before the pool goes. The pool holds the handler and
            // the handler holds the signal the wait is on: a pool closed with the handler still
            // registered is a pool whose worker thread may raise it once more.
            if let Err(e) = session.pool.RemoveFrameArrived(session.arrival) {
                tracing::warn!("removing the WGC frame-arrival handler failed: {e}");
            }
            if let Err(e) = session.pool.Close() {
                tracing::warn!("closing the WGC frame pool failed: {e}");
            }
        }
        self.release_com();
        Ok(())
    }

    /// Ask for textures rather than pixels, forwarding to the session when there is one.
    ///
    /// Remembered even with no session, because the caller asks between construction and `start`,
    /// when the encoder is configured; [`Self::start_session`] carries it into the session it builds.
    fn deliver_textures(&mut self, textures: bool) {
        self.want_textures = textures;
        if let Some(session) = self.session.as_mut() {
            session.deliver_textures(textures);
        }
    }

    fn frame_timings(&self) -> Option<crate::FrameTimings> {
        Some(crate::FrameTimings {
            waiting: self.waiting,
            copying: self.copying,
        })
    }

    fn native_size(&self) -> (u32, u32) {
        // Invariant owed to the caller: this is the size of every frame `next_frame` produces, since
        // both come from the capture item (the pool is created with `item.Size()`). The rawvideo pipe
        // is declared from it, so a mismatch is fatal; see `copy_out` and the CLI's frame guard.
        self.size
    }

    /// Close everything the frame pool holds without reading a pixel back; see [`Session::discard_pending`].
    ///
    /// The CLI's pacer calls this instead of `next_frame` when it has no slot for a frame (~45% of
    /// frames on the measured 4K machine, where the compositor delivered 53-75fps against 30).
    /// Reading those back cost 33.2MB of GPU copy plus a row-by-row CPU copy each. A frame must still
    /// be closed: the pool holds only `BUFFER_COUNT`, and an unclosed frame starves the next capture.
    fn discard_pending(&mut self) -> Result<usize> {
        // Same contract as `next_frame`: without `start` there is no session to drain, and reporting
        // success would hide that.
        if !self.started {
            bail!("WGC capture is not started");
        }
        let session = self
            .session
            .as_ref()
            .context("WGC capture has no session (already stopped)")?;
        // `&Session`: a read of the pool, not a change to the session; frames are released through their own handles.
        session.discard_pending()
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        // A backend dropped without `stop` must still shut the capture down and give the COM apartment back.
        self.session = None;
        self.release_com();
    }
}

impl Session {
    /// Build the pool, the session and the frame-arrival registration.
    ///
    /// `signal` is the latch the wait blocks on; the pool's own `FrameArrived` event is what
    /// raises it.
    fn new(target: &CaptureTarget, signal: Arc<FrameSignal>) -> Result<Self> {
        let (device, context) = create_device()?;
        let item = create_capture_item(target)?;
        let item_size = item.Size().context("GraphicsCaptureItem::Size")?;
        let size = pixel_size(item_size)?;
        match item.DisplayName() {
            Ok(name) => tracing::info!(display = %name, ?target, "capturing"),
            Err(e) => tracing::debug!("the capture item has no display name: {e}"),
        }
        let winrt_device = winrt_device(&device)?;

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            // The encoder pipe is BGRA8; the pool must produce the same layout.
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            BUFFER_COUNT,
            item_size,
        )
        .context("Direct3D11CaptureFramePool::CreateFreeThreaded")?;
        // The pool says when a frame has arrived, and this is the only thing that acts on it: set
        // the flag and wake the wait. The handler runs on the pool's own worker thread (a
        // free-threaded pool has no dispatcher queue), so it must be cheap and must not block.
        //
        // It holds the signal and nothing else, so there is no cycle back to the pool: the pool
        // holds the handler, the handler holds the signal, and the signal holds nothing.
        let handler_signal = Arc::clone(&signal);
        let handler: TypedEventHandler<Direct3D11CaptureFramePool, IInspectable> =
            TypedEventHandler::new(move |_sender, _args| -> windows::core::Result<()> {
                handler_signal.arrive();
                Ok(())
            });
        // The handler is not kept: the pool holds its own reference to the delegate, and the
        // delegate type is not `Send`, so a session that kept one could not be sent.
        let arrival = pool
            .FrameArrived(&handler)
            .context("Direct3D11CaptureFramePool::FrameArrived")?;
        let capture = pool
            .CreateCaptureSession(&item)
            .context("Direct3D11CaptureFramePool::CreateCaptureSession")?;
        // Both are best effort: they need newer interface revisions than the capture itself and fail
        // harmlessly (a missing cursor, or the yellow border Windows 11 draws for untrusted apps).
        if let Err(e) = capture.SetIsCursorCaptureEnabled(true) {
            tracing::debug!("cursor capture could not be enabled: {e}");
        }
        if let Err(e) = capture.SetIsBorderRequired(false) {
            tracing::debug!("the capture border could not be disabled: {e}");
        }
        capture
            .StartCapture()
            .context("GraphicsCaptureSession::StartCapture")?;
        tracing::info!(width = size.0, height = size.1, "WGC capture started");

        Ok(Self {
            device,
            context,
            pool,
            arrival,
            size,
            _item: item,
            capture,
            staging: None,
            handover: None,
        })
    }

    /// One poll of the frame pool. `Ok(None)` means nothing is available yet.
    fn poll_frame(&mut self) -> Result<Option<Frame>> {
        let frame = match self.pool.TryGetNextFrame() {
            Ok(frame) => frame,
            // `TryGetNextFrame` reports "no frames ready" as S_OK with a null frame, which this binding
            // turns into `Error::empty()` (HRESULT still S_OK). Anything else is a genuine failure.
            Err(e) if is_frame_unavailable(&e) => return Ok(None),
            Err(e) => bail!("Direct3D11CaptureFramePool::TryGetNextFrame failed: {e}"),
        };

        // Same clock base as the audio backend, so A/V alignment is derivable. `elapsed` is monotonic,
        // which is all `Frame::pts` promises.
        let pts = clock_base().elapsed();
        let staged = self.copy_out(&frame, pts);
        // Release the frame whether or not the copy worked, or the pool starves after BUFFER_COUNT frames.
        let closed = frame.Close();
        let frame = staged?;
        closed.context("closing the WGC frame")?;
        Ok(Some(frame))
    }

    /// Close every frame the pool holds without reading a pixel back, and return how many.
    ///
    /// [`Self::poll_frame`] minus its cost: no staging texture, `CopyResource`, `Map` or allocation;
    /// only `TryGetNextFrame` and `Close` per frame.
    ///
    /// Closing is not optional: a frame taken from the pool but never closed keeps its buffer, and
    /// `BUFFER_COUNT` of them starve the capture. As far as WGC is concerned the frame was consumed.
    ///
    /// "No frame ready" is handled as in `poll_frame` ([`is_frame_unavailable`]); any other error is
    /// reported, because a drain that silently stopped working would look like an idle source.
    fn discard_pending(&self) -> Result<usize> {
        let mut discarded = 0;
        // Bounded by [`DISCARD_LIMIT`]. Hitting it is not a failure: frames taken are closed and the
        // next call drains the rest.
        for _ in 0..DISCARD_LIMIT {
            let frame = match self.pool.TryGetNextFrame() {
                Ok(frame) => frame,
                Err(e) if is_frame_unavailable(&e) => break,
                Err(e) => {
                    bail!("Direct3D11CaptureFramePool::TryGetNextFrame failed while draining: {e}")
                }
            };
            // SAFETY: `frame` is a live `Direct3D11CaptureFrame` from this session's own frame pool, owned
            // by this thread's pool and nothing else; `Close` is the documented release and runs exactly once
            // per frame. Nothing below reads the frame's texture or surface, so no device, context or mapped
            // memory is involved and there is nothing to synchronise with. Closing releases the pool buffer,
            // which lets the pool deliver the next frame (`poll_frame` does the same for the frame it copies).
            frame.Close().context("closing a discarded WGC frame")?;
            discarded += 1;
        }
        Ok(discarded)
    }

    /// Stage-copy the frame's D3D11 texture into CPU-visible memory as BGRA8.
    fn copy_out(&mut self, frame: &Direct3D11CaptureFrame, pts: Duration) -> Result<Frame> {
        let surface: IDirect3DSurface =
            frame.Surface().context("Direct3D11CaptureFrame::Surface")?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        // SAFETY: `access` is the frame's own surface; `GetInterface` is the documented way to reach the
        // underlying D3D11 texture from it.
        let texture: ID3D11Texture2D = unsafe { access.GetInterface() }?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `desc` is a valid out-parameter for this texture.
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            // The encoder pipe is fixed to `bgra`; a different pixel layout would give colour-shifted
            // video, so refuse the frame and say why.
            bail!(
                "WGC delivered a {:?} frame but the encoder pipe is BGRA8; not converting",
                desc.Format
            );
        }
        let (width, height) = (desc.Width, desc.Height);

        // Handed over rather than read back when textures were asked for: no staging texture, `Map` or
        // row copy; the pixels stay in VRAM and only a COM reference crosses.
        if self.handover.is_some() {
            let owned = self.handover_texture(&texture)?;
            return Ok(Frame {
                data: Vec::new(),
                pts,
                width,
                height,
                format: PixelFormat::Bgra8,
                texture: Some(crate::GpuTexture::new(owned)),
            });
        }

        let row_bytes = width as usize * 4;
        let staging = self.staging_texture(width, height, desc.Format)?;
        let dst: &ID3D11Resource = &staging;

        // SAFETY: the immediate context, the captured texture and the staging texture all belong to the
        // same device (`create_device` builds the one device the frame pool, hence `texture`, belongs
        // to; `staging_texture` builds the staging copy on it). This device-side copy is what keeps the
        // map below from reading a freshly created, zero-filled texture (the black-frame bug). It is
        // issued on the same immediate context the `Map` below is ordered against, so the copy is
        // complete before the mapped bytes are read. The textures match in size, format, mip level,
        // array size and sample count, which makes the whole-resource copy valid.
        unsafe { self.context.CopyResource(dst, &texture) };

        let mut data = vec![0u8; row_bytes * height as usize];

        // SAFETY: as above, `dst` belongs to this immediate context's device. `Map` is checked before
        // the copy and the `Unmap` below runs on every path that successfully mapped.
        let mapped = unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map(|()| mapped)
        };
        let mapped = match mapped {
            Ok(mapped) => mapped,
            Err(e) => bail!("mapping the WGC staging texture failed: {e}"),
        };
        if mapped.pData.is_null() {
            // SAFETY: paired with the successful `Map` above.
            unsafe { self.context.Unmap(dst, 0) };
            bail!("D3D11 Map returned a null data pointer for the WGC staging texture");
        }
        // SAFETY: `pData` points at a mapped staging texture holding at least `RowPitch * height`
        // readable bytes; `data` is exactly `row_bytes * height` long, so each row copy stays inside both
        // allocations. The mapped texture is never bound to a pipeline stage, so nothing else observes
        // it while mapped.
        unsafe {
            for y in 0..height as usize {
                // Rows are `RowPitch` apart, >= the row length, so this is a row-by-row copy, not one memcpy.
                let src = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                let row = data.as_mut_ptr().add(y * row_bytes);
                std::ptr::copy_nonoverlapping(src, row, row_bytes);
            }
            self.context.Unmap(dst, 0);
        }

        Ok(Frame {
            data,
            pts,
            width,
            height,
            format: PixelFormat::Bgra8,
            texture: None,
        })
    }

    /// Ask this session to hand out textures rather than pixels.
    ///
    /// The ring is built on first use, so a session never asked pays nothing for its 33 MB-at-4K buffers.
    fn deliver_textures(&mut self, textures: bool) {
        if textures && self.handover.is_none() {
            self.handover = Some(Handover::default());
        }
        if !textures {
            self.handover = None;
        }
    }

    /// A texture to hand over, in ring order, copied from the frame's own surface.
    ///
    /// `CopyResource` moves the 33 MB on the GPU in microseconds, where the staging copy and row-by-row
    /// `Map` it replaces moved the same bytes through system memory per frame.
    fn handover_texture(&mut self, source: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `desc` is a valid out-parameter for `source`.
        unsafe { source.GetDesc(&mut desc) };
        let ring = self
            .handover
            .get_or_insert_with(Handover::default)
            .texture(&self.device, desc.Width, desc.Height, desc.Format)?;
        let dst: &ID3D11Resource = &ring;
        // SAFETY: both textures are on this session's device and match in size, format, mip level, array
        // size and sample count, which makes a whole-resource copy valid. It is issued on the immediate
        // context, the same one the encoder's manager was given, so it is ordered ahead of the encoder reading it.
        unsafe { self.context.CopyResource(dst, source) };
        Ok(ring)
    }

    fn staging_texture(
        &mut self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> Result<ID3D11Texture2D> {
        let reusable = match &self.staging {
            Some((w, h, texture)) => {
                let mut current = D3D11_TEXTURE2D_DESC::default();
                // SAFETY: `current` is a valid out-parameter for `texture`.
                unsafe { texture.GetDesc(&mut current) };
                *w == width && *h == height && current.Format == format
            }
            None => false,
        };
        if !reusable {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: format,
                // WGC textures are never multisampled, and a staging copy must match the source's sample description.
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut created: Option<ID3D11Texture2D> = None;
            // SAFETY: `desc` is fully initialised, and a staging texture needs no initial data.
            unsafe {
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut created))
                    .context("creating the WGC staging texture")?;
            }
            let created = created.context("CreateTexture2D returned no texture")?;
            tracing::debug!(width, height, "created the WGC staging texture");
            self.staging = Some((width, height, created));
        }
        self.staging
            .as_ref()
            .map(|(_, _, texture)| texture.clone())
            .context("staging texture vanished")
    }
}

/// `TryGetNextFrame` signals "no frames ready" with S_OK plus a null frame, which this binding
/// surfaces as an `Err` whose HRESULT is still S_OK (`Error::empty()`).
fn is_frame_unavailable(e: &windows::core::Error) -> bool {
    e.code().is_ok()
}

/// D3D11 device + immediate context, hardware driver, BGRA-capable.
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: both out-parameters are valid `Option` slots; `None` for the adapter and the
    // feature-level list means "pick the default hardware device".
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            // WGC requires a BGRA-capable device.
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice(D3D_DRIVER_TYPE_HARDWARE)")?;
    }
    let device = device.context("D3D11CreateDevice returned no device")?;
    let context = context.context("D3D11CreateDevice returned no immediate context")?;

    // Thread-safe use of this device, which several things share: WGC drives its own internals
    // against it, this process issues frame copies on its pump thread, and an asynchronous hardware
    // encoder MFT reads frames on threads of its own. Without this call D3D11 leaves concurrent use
    // from several threads to the caller; with it the runtime serialises and the arrangement is
    // defined. The lock costs nothing beside a 33 MB copy, and the alternative is torn frames on one
    // machine and never on another.
    // SAFETY: `device` is a live D3D11 device and `ID3D11Multithread` is the interface D3D11 exposes
    // for exactly this setting. A device that does not implement it is not worth failing capture
    // over, so the cast's error is reported and stepped over.
    match device.cast::<ID3D11Multithread>() {
        Ok(multithread) => {
            // The return value is the previous setting, which this device never set: informational, dropped.
            // SAFETY: `multithread` belongs to this device.
            unsafe {
                let _ = multithread.SetMultithreadProtected(true);
            }
        }
        Err(err) => tracing::warn!(
            "this D3D11 device does not expose ID3D11Multithread ({err}), so concurrent use \
             of it is not protected"
        ),
    }

    Ok((device, context))
}

/// Wrap the D3D11 device for WinRT, which is what the frame pool takes.
fn winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice> {
    let dxgi_device: IDXGIDevice = device
        .cast()
        .context("casting ID3D11Device to IDXGIDevice")?;
    // SAFETY: `dxgi_device` is live, and the returned IInspectable is the WinRT projection of that same device.
    let inspectable = unsafe {
        CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)
            .context("CreateDirect3D11DeviceFromDXGIDevice")?
    };
    inspectable
        .cast()
        .context("casting to Windows.Graphics.DirectX.Direct3D11.IDirect3DDevice")
}

/// The capture item for the target: a window when `[capture] window` names one, else the monitor
/// at `[capture] monitor` (0 = primary).
///
/// Monitors have two routes, tried in turn because they fail in different places:
///
/// 1. `IGraphicsCaptureItemInterop::CreateForMonitor`, the pre-Windows-11 interop interface. It
///    works for unpackaged Win32 apps but has been reported to fail with `E_INVALIDARG` on some
///    recent Windows 11 builds.
/// 2. `GraphicsCaptureItem::TryCreateFromDisplayId` (Windows 11 22H2+), which needs
///    `GraphicsCaptureAccess::RequestAccessAsync(Programmatic)` and, for packaged apps, the
///    `graphicsCaptureProgrammatic` manifest capability. The `DisplayId` is understood to wrap
///    the `HMONITOR` (the mapping the Windows App SDK's `GetDisplayIdFromMonitor` performs), so
///    the handle is used directly; `QueryDisplayConfig`'s LUID/target-id values are a different token.
///
/// A window uses `IGraphicsCaptureItemInterop::CreateForWindow` only. Neither monitor route has
/// been executed on Windows.
fn create_capture_item(target: &CaptureTarget) -> Result<GraphicsCaptureItem> {
    if let Some(title) = &target.window {
        let candidates = visible_windows();
        let hwnd = pick_window(&candidates, title)?;
        return create_item_for_window(hwnd);
    }
    // SAFETY: a null HWND with MONITOR_DEFAULTTOPRIMARY is the documented way to ask for the primary
    // monitor, and it touches nothing the caller owns.
    let primary = unsafe { MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY) };
    if primary.is_invalid() {
        bail!("MonitorFromWindow could not resolve the primary monitor");
    }
    let monitor = if target.monitor == 0 {
        primary
    } else {
        pick_monitor(
            &ordered_monitors(primary, &enumerate_monitors()),
            target.monitor,
        )?
    };

    match create_item_via_interop(monitor) {
        Ok(item) => Ok(item),
        Err(interop_err) => match create_item_via_display_id(monitor) {
            Ok(item) => Ok(item),
            Err(display_err) => bail!(
                "no GraphicsCaptureItem for monitor {}: the interop path failed \
                 ({interop_err:#}); the DisplayId path failed ({display_err:#})",
                target.monitor
            ),
        },
    }
}

/// Every monitor, in the OS's enumeration order.
fn enumerate_monitors() -> Vec<HMONITOR> {
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _dc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: `data` is the `&mut Vec<HMONITOR>` passed below, alive for the whole call.
        let out = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        out.push(monitor);
        BOOL(1)
    }
    let mut monitors: Vec<HMONITOR> = Vec::new();
    // SAFETY: the callback only pushes into `monitors`, which outlives the synchronous call.
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut monitors as *mut Vec<HMONITOR> as isize),
        );
    }
    monitors
}

/// Visible, titled, top-level windows, topmost first (`EnumWindows` order).
fn visible_windows() -> Vec<(HWND, String)> {
    unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `&mut Vec` passed below, alive for the whole call.
        let out = unsafe { &mut *(data.0 as *mut Vec<(HWND, String)>) };
        // SAFETY: plain queries on a handle the OS just handed us.
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return BOOL(1);
            }
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return BOOL(1);
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(hwnd, &mut buf);
            if n > 0 {
                out.push((hwnd, String::from_utf16_lossy(&buf[..n as usize])));
            }
        }
        BOOL(1)
    }
    let mut windows: Vec<(HWND, String)> = Vec::new();
    // SAFETY: the callback only pushes into `windows`, which outlives the synchronous call.
    unsafe {
        let _ = EnumWindows(
            Some(collect),
            LPARAM(&mut windows as *mut Vec<(HWND, String)> as isize),
        );
    }
    windows
}

/// `IGraphicsCaptureItemInterop::CreateForWindow`.
fn create_item_for_window(hwnd: HWND) -> Result<GraphicsCaptureItem> {
    let interop: IGraphicsCaptureItemInterop =
        factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .context("activating IGraphicsCaptureItemInterop for GraphicsCaptureItem")?;
    // SAFETY: `hwnd` came from EnumWindows moments ago; a stale handle is an error return.
    unsafe {
        interop
            .CreateForWindow(hwnd)
            .context("IGraphicsCaptureItemInterop::CreateForWindow")
    }
}

/// Route 1: the `IGraphicsCaptureItemInterop` factory.
fn create_item_via_interop(monitor: HMONITOR) -> Result<GraphicsCaptureItem> {
    let interop: IGraphicsCaptureItemInterop =
        factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .context("activating IGraphicsCaptureItemInterop for GraphicsCaptureItem")?;
    // SAFETY: `monitor` came from MonitorFromWindow, and the created item is requested with the interface's own IID.
    unsafe {
        interop
            .CreateForMonitor(monitor)
            .context("IGraphicsCaptureItemInterop::CreateForMonitor")
    }
}

/// Route 2: `DisplayId` (Windows 11 22H2+), gated on the programmatic-access grant.
fn create_item_via_display_id(monitor: HMONITOR) -> Result<GraphicsCaptureItem> {
    let status = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Programmatic)
        .context("GraphicsCaptureAccess::RequestAccessAsync(Programmatic)")?
        .join()
        .context("waiting for the graphics capture access request")?;
    if status != AppCapabilityAccessStatus::Allowed {
        bail!("programmatic graphics capture access was not granted ({status:?})");
    }
    // HMONITOR is a 32-bit value the OS sign-extends to pointer size, so mask to 32 bits before
    // widening again for the DisplayId's u64.
    let value = monitor.0 as usize as u32 as u64;
    GraphicsCaptureItem::TryCreateFromDisplayId(DisplayId { Value: value })
        .context("GraphicsCaptureItem::TryCreateFromDisplayId for the primary monitor")
}

/// The capture item's size as whole physical pixels.
///
/// `GraphicsCaptureItem::Size()` is what is actually captured, in physical pixels, and the value
/// the frame pool is created with. `GetSystemMetrics(SM_CXSCREEN/SM_CYSCREEN)` is no substitute:
/// it is DPI-virtualised, so a process that is not per-monitor DPI aware gets logical pixels
/// (2560x1440 for a 4K desktop at 150% scaling while the item was 3840x2160), which declares the
/// rawvideo pipe differently from the frames arriving on it.
fn pixel_size(size: SizeInt32) -> Result<(u32, u32)> {
    if size.Width < 1 || size.Height < 1 {
        bail!(
            "the capture item reported a {}x{} physical size",
            size.Width,
            size.Height
        );
    }
    Ok((size.Width as u32, size.Height as u32))
}
