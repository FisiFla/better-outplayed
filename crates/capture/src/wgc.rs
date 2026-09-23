//! Windows Graphics Capture (WGC) video backend.
//!
//! WGC hands us one D3D11 texture per captured frame; we copy that texture into system
//! memory as BGRA8. That copy is the known cost of the ffmpeg-sidecar encode path
//! (spec §6.1) and is exactly what a Phase 2 Media Foundation backend would remove.
//!
//! # What has been verified about this code
//!
//! It type-checks for `x86_64-pc-windows-msvc` from macOS (`cargo check` does not
//! link), which is a real gate for API shape, ownership and HRESULT plumbing. It has
//! **never been executed on Windows**: no frame has ever come through it, and neither
//! the pixel data, the frame pacing nor the caption/format of what the compositor
//! hands back has been observed. Task 17's runbook is the first place this gets a run.
//!
//! # Frame arrival
//!
//! `CaptureBackend::next_frame` is a pull API, so this backend polls
//! `Direct3D11CaptureFramePool::TryGetNextFrame` on the caller's thread instead of
//! registering a `FrameArrived` handler and hopping through a channel. The pool is
//! created free-threaded, which is what makes polling it without a dispatcher legal.
//!
//! # Known gaps
//!
//! - A display mode change mid-capture normally needs
//!   `Direct3D11CaptureFramePool::Recreate`. This backend does not call it: it copies
//!   whatever size the incoming texture actually is (re-creating its staging texture
//!   when that changes) and reports that size, but the pool itself is never resized.
//! - Only the primary monitor is captured, and monitor selection is not wired to
//!   configuration.
//! - Frames are copied through a staging texture on every frame. That is the cost this
//!   design accepts to keep the encoder an ffmpeg sidecar (spec §6.1).

#![cfg(windows)]

use crate::platform::init_com;
use crate::{clock_base, CaptureBackend, Frame, PixelFormat};
use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};

use windows::core::{factory, Interface};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureAccess,
    GraphicsCaptureAccessKind, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::{IDirect3DDevice, IDirect3DSurface};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::DisplayId;
use windows::Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::Win32::Graphics::Gdi::{HMONITOR, MonitorFromWindow, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::Com::CoUninitialize;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

/// Buffers in the frame pool. A pull consumer that copies every frame into system
/// memory before asking for the next one needs no more than a couple.
const BUFFER_COUNT: i32 = 2;

/// How long to sleep between polls while waiting for a frame. WGC has no blocking wait
/// outside the `FrameArrived` callback, and polling at 1ms keeps the added latency well
/// under a single 60fps frame interval.
const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Primary-monitor screen capture.
pub struct WgcCapture {
    /// Only monitor 0 (the primary monitor) is supported; `create_capture_item` says
    /// so explicitly rather than quietly capturing the wrong display.
    monitor_index: usize,
    width: u32,
    height: u32,
    session: Option<Session>,
    /// Whether this thread's COM apartment was taken by us and must be given back.
    com_owned: bool,
}

/// Everything `start` acquires, so there is one thing to release.
struct Session {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pool: Direct3D11CaptureFramePool,
    /// Held deliberately: the pool and the session capture *this* item, and dropping
    /// our reference to it while they are running is not something WGC documents as
    /// safe. Nothing below `start` reads it, hence the underscore.
    _item: GraphicsCaptureItem,
    /// Closed by `stop`; kept for the lifetime of the capture otherwise.
    capture: GraphicsCaptureSession,
    /// Re-created when the frame size or format changes (resolution switch).
    staging: Option<Staging>,
}

type Staging = (u32, u32, ID3D11Texture2D);

impl WgcCapture {
    /// `monitor_index` is 0 for the primary monitor.
    pub fn new(monitor_index: usize) -> Result<Self> {
        let (width, height) = primary_monitor_size()?;
        Ok(Self { monitor_index, width, height, session: None, com_owned: false })
    }

    /// Primary-monitor dimensions as reported by `GetSystemMetrics`.
    ///
    /// A WGC frame carries its own size: at a different resolution or scale the texture
    /// can differ from this, and `next_frame` always reports the size it actually
    /// copied.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn release_com(&mut self) {
        if self.com_owned {
            // SAFETY: `start` took the apartment on this thread.
            unsafe { CoUninitialize() };
            self.com_owned = false;
        }
    }
}

impl CaptureBackend for WgcCapture {
    fn start(&mut self) -> Result<()> {
        if self.session.is_some() {
            bail!("WGC capture is already started");
        }
        if self.monitor_index != 0 {
            bail!(
                "monitor {} was requested, but localplay only captures the primary \
                 monitor (index 0)",
                self.monitor_index
            );
        }
        self.com_owned = init_com()?;
        match Session::new() {
            Ok(session) => {
                self.session = Some(session);
                Ok(())
            }
            Err(e) => {
                // No frame was captured, so hand the apartment back rather than
                // leaving the thread in an apartment nobody owns.
                self.release_com();
                Err(e)
            }
        }
    }

    /// Polls for the next frame. Returns `Ok(None)` when no frame arrives within
    /// `timeout`, per the `CaptureBackend` contract.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        let session = self.session.as_mut().context("WGC capture is not started")?;
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(frame) = session.poll_frame()? {
                return Ok(Some(frame));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            std::thread::sleep(POLL_INTERVAL.min(deadline - now));
        }
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(session) = self.session.take() {
            // Close the session before the pool, which is the order every WGC sample
            // uses, and say so if either refuses rather than pretending it stopped.
            if let Err(e) = session.capture.Close() {
                tracing::warn!("closing the WGC capture session failed: {e}");
            }
            if let Err(e) = session.pool.Close() {
                tracing::warn!("closing the WGC frame pool failed: {e}");
            }
        }
        self.release_com();
        Ok(())
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        // A backend that is dropped without `stop` must still shut the capture down
        // and give the COM apartment back.
        self.session = None;
        self.release_com();
    }
}

impl Session {
    fn new() -> Result<Self> {
        let (device, context) = create_device()?;
        let item = create_capture_item()?;
        let size = item.Size().context("GraphicsCaptureItem::Size")?;
        match item.DisplayName() {
            Ok(name) => tracing::info!(display = %name, "capturing the primary monitor"),
            Err(e) => tracing::debug!("the capture item has no display name: {e}"),
        }
        let winrt_device = winrt_device(&device)?;

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            // The encoder pipe is BGRA8; the pool must produce the same layout.
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            BUFFER_COUNT,
            size,
        )
        .context("Direct3D11CaptureFramePool::CreateFreeThreaded")?;
        let capture = pool
            .CreateCaptureSession(&item)
            .context("Direct3D11CaptureFramePool::CreateCaptureSession")?;
        // Both of these are best effort: they need newer interface revisions than the
        // capture itself and fail harmlessly (a missing cursor, or the yellow border
        // that Windows 11 draws when it does not consider the app trusted).
        if let Err(e) = capture.SetIsCursorCaptureEnabled(true) {
            tracing::debug!("cursor capture could not be enabled: {e}");
        }
        if let Err(e) = capture.SetIsBorderRequired(false) {
            tracing::debug!("the capture border could not be disabled: {e}");
        }
        capture.StartCapture().context("GraphicsCaptureSession::StartCapture")?;
        tracing::info!(
            width = size.Width,
            height = size.Height,
            "WGC capture started on the primary monitor"
        );

        Ok(Self { device, context, pool, _item: item, capture, staging: None })
    }

    /// One poll of the frame pool. `Ok(None)` means "nothing available yet".
    fn poll_frame(&mut self) -> Result<Option<Frame>> {
        let frame = match self.pool.TryGetNextFrame() {
            Ok(frame) => frame,
            // `TryGetNextFrame` reports "no frames are ready" as S_OK with a null
            // frame, and this binding turns a null interface into `Error::empty()`,
            // whose HRESULT is still S_OK. Anything else is a genuine failure.
            Err(e) if is_frame_unavailable(&e) => return Ok(None),
            Err(e) => bail!("Direct3D11CaptureFramePool::TryGetNextFrame failed: {e}"),
        };

        // Same clock base as the audio backend, so A/V alignment is derivable
        // (spec §5.1). `elapsed` is monotonic, which is all `Frame::pts` promises.
        let pts = clock_base().elapsed();
        let staged = self.copy_out(&frame, pts);
        // The frame must be released whether or not the copy worked, or the pool
        // starves after BUFFER_COUNT frames.
        let closed = frame.Close();
        let frame = staged?;
        closed.context("closing the WGC frame")?;
        Ok(Some(frame))
    }

    /// Stage-copy the frame's D3D11 texture into CPU-visible memory as BGRA8.
    fn copy_out(&mut self, frame: &Direct3D11CaptureFrame, pts: Duration) -> Result<Frame> {
        let surface: IDirect3DSurface = frame.Surface().context("Direct3D11CaptureFrame::Surface")?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        // SAFETY: `access` is the frame's own surface; `GetInterface` is the
        // documented way to reach the underlying D3D11 texture from it.
        let texture: ID3D11Texture2D = unsafe { access.GetInterface() }?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `desc` is a valid out-parameter for this texture.
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            // The encoder pipe is fixed to `bgra`; silently different pixel layouts
            // would produce colour-shifted video, so refuse the frame and say why.
            bail!(
                "WGC delivered a {:?} frame but the encoder pipe is BGRA8; not converting",
                desc.Format
            );
        }
        let (width, height) = (desc.Width, desc.Height);
        let row_bytes = width as usize * 4;
        let staging = self.staging_texture(width, height, desc.Format)?;
        let mut data = vec![0u8; row_bytes * height as usize];

        // SAFETY: the immediate context, the source texture and the staging texture
        // all belong to the same device. `Map` is checked before the copy and the
        // `Unmap` below runs on every path that successfully mapped.
        let dst: &ID3D11Resource = &staging;
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
        // SAFETY: `pData` points at a mapped staging texture holding at least
        // `RowPitch * height` readable bytes; `data` is exactly `row_bytes * height`
        // bytes long, so each row copy stays inside both allocations. The mapped
        // texture is never bound to a pipeline stage, so nothing else observes it
        // while it is mapped.
        unsafe {
            for y in 0..height as usize {
                // Rows are `RowPitch` apart in the staging texture, which is >= the
                // row length, so this is a row-by-row copy rather than one memcpy.
                let src = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                let row = data.as_mut_ptr().add(y * row_bytes);
                std::ptr::copy_nonoverlapping(src, row, row_bytes);
            }
            self.context.Unmap(dst, 0);
        }

        Ok(Frame { data, pts, width, height, format: PixelFormat::Bgra8 })
    }

    /// The CPU-readable staging texture, created once per (size, format).
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
                // WGC textures are never multisampled, and a staging copy has to
                // match the source's sample description.
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut created: Option<ID3D11Texture2D> = None;
            // SAFETY: `desc` is fully initialised, and a staging texture needs no
            // initial data.
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

/// `TryGetNextFrame` signals "no frames are ready" with S_OK plus a null frame, which
/// this binding surfaces as an `Err` whose HRESULT is still S_OK (`Error::empty()`).
fn is_frame_unavailable(e: &windows::core::Error) -> bool {
    e.code().is_ok()
}

/// D3D11 device + immediate context, hardware driver, BGRA-capable.
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: both out-parameters are valid `Option` slots; `None` for the adapter and
    // for the feature-level list means "pick the default hardware device".
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
    Ok((device, context))
}

/// Wrap the D3D11 device for WinRT, which is what the frame pool takes.
fn winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice> {
    let dxgi_device: IDXGIDevice = device.cast().context("casting ID3D11Device to IDXGIDevice")?;
    // SAFETY: `dxgi_device` is live, and the returned IInspectable is the WinRT
    // projection of that same device.
    let inspectable = unsafe {
        CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)
            .context("CreateDirect3D11DeviceFromDXGIDevice")?
    };
    inspectable
        .cast()
        .context("casting to Windows.Graphics.DirectX.Direct3D11.IDirect3DDevice")
}

/// The capture item for the primary monitor.
///
/// Two routes exist and this tries both, because they fail in different places:
///
/// 1. `IGraphicsCaptureItemInterop::CreateForMonitor`, the pre-Windows-11 interop
///    interface. It works for unpackaged Win32 apps, which is what this is, but has
///    been reported to fail with `E_INVALIDARG` on some recent Windows 11 builds.
/// 2. `GraphicsCaptureItem::TryCreateFromDisplayId` (Windows 11 22H2+), which requires
///    `GraphicsCaptureAccess::RequestAccessAsync(Programmatic)` and, for packaged
///    apps, the `graphicsCaptureProgrammatic` manifest capability. The `DisplayId` is
///    understood to wrap the monitor `HMONITOR` — that is the mapping the Windows App
///    SDK's `GetDisplayIdFromMonitor` performs — which is why the handle is used
///    directly here instead of being reconstructed from `QueryDisplayConfig`, whose
///    LUID/target-id values are a different token entirely.
///
/// Neither route has been executed on Windows; see the module docs.
fn create_capture_item() -> Result<GraphicsCaptureItem> {
    // SAFETY: a null HWND with MONITOR_DEFAULTTOPRIMARY is the documented way to ask
    // for the primary monitor, and it touches nothing the caller owns.
    let monitor = unsafe { MonitorFromWindow(None::<&HWND>, MONITOR_DEFAULTTOPRIMARY) };
    if monitor.is_invalid() {
        bail!("MonitorFromWindow could not resolve the primary monitor");
    }

    match create_item_via_interop(monitor) {
        Ok(item) => Ok(item),
        Err(interop_err) => match create_item_via_display_id(monitor) {
            Ok(item) => Ok(item),
            Err(display_err) => bail!(
                "no GraphicsCaptureItem for the primary monitor: the interop path failed \
                 ({interop_err:#}); the DisplayId path failed ({display_err:#})"
            ),
        },
    }
}

/// Route 1: the `IGraphicsCaptureItemInterop` factory.
fn create_item_via_interop(monitor: HMONITOR) -> Result<GraphicsCaptureItem> {
    let interop: IGraphicsCaptureItemInterop =
        factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .context("activating IGraphicsCaptureItemInterop for GraphicsCaptureItem")?;
    // SAFETY: `monitor` came from MonitorFromWindow, and the created item is
    // requested with the interface's own IID.
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
        .get()
        .context("waiting for the graphics capture access request")?;
    if status != AppCapabilityAccessStatus::Allowed {
        bail!("programmatic graphics capture access was not granted ({status:?})");
    }
    // HMONITOR is a 32-bit value that the OS sign-extends to pointer size, so mask
    // back down to 32 bits before widening again for the DisplayId's u64.
    let value = monitor.0 as usize as u32 as u64;
    GraphicsCaptureItem::TryCreateFromDisplayId(DisplayId { Value: value })
        .context("GraphicsCaptureItem::TryCreateFromDisplayId for the primary monitor")
}

/// Primary-monitor dimensions from `GetSystemMetrics(SM_CXSCREEN/SM_CYSCREEN)`.
fn primary_monitor_size() -> Result<(u32, u32)> {
    // SAFETY: GetSystemMetrics is a pure query.
    let (width, height) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    if width <= 0 || height <= 0 {
        bail!("GetSystemMetrics reported a {width}x{height} primary monitor");
    }
    Ok((width as u32, height as u32))
}
