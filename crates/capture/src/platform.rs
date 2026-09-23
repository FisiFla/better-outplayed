//! Backend selection, so every `#[cfg(windows)]` line lives inside this crate.
//!
//! `cargo check --target x86_64-pc-windows-msvc` can type-check this crate from a
//! non-Windows host (`cargo check` does not link), which is the only Windows gate
//! available without Windows hardware. Keeping the selection here rather than in the
//! CLI is what makes that gate cover the choice as well as the backends.
//!
//! # No silent stub fallback on Windows
//!
//! On Windows these functions either return a real backend or fail. There is
//! deliberately **no** fallback to `StubCapture`/`StubAudio`: a synthetic stand-in would
//! make the application look like it is buffering while capturing nothing, and the
//! failure would only surface much later as an empty clip. The stubs exist for
//! non-Windows development, and that is the only place they are selected.

use crate::stub::StubConfig;
use crate::{AudioBackend, AudioFormat, CaptureBackend};
use anyhow::Result;

/// The video backend for this platform.
///
/// `stub` describes the synthetic source used off Windows, and on Windows it is only
/// read to check the encoder's expected frame size against the size WGC will actually
/// deliver — see the warning below.
pub fn default_video_backend(stub: StubConfig) -> Result<Box<dyn CaptureBackend>> {
    #[cfg(windows)]
    {
        let capture = crate::wgc::WgcCapture::new(0)?;
        let (width, height) = capture.size();
        if (width, height) != (stub.width, stub.height) {
            // ffmpeg's rawvideo input is declared with the configured `encode.output_size`,
            // so a monitor that does not match corrupts the stream rather than being
            // scaled. Say so at startup instead of producing garbage segments.
            tracing::warn!(
                "the primary monitor is {width}x{height} but encoding is configured for \
                 {}x{}; set `encode.output_size` to {width}x{height} (or leave it empty \
                 once native-size encoding lands) or the raw video pipe will be mis-read",
                stub.width,
                stub.height
            );
        }
        Ok(Box::new(capture))
    }
    #[cfg(not(windows))]
    {
        Ok(Box::new(crate::stub::StubCapture::new(stub)))
    }
}

/// The audio backend for this platform.
///
/// `format` is what the synthetic source produces off Windows. On Windows the WASAPI
/// backend always produces `AudioFormat::default()` (48kHz stereo s16le) and refuses an
/// endpoint that cannot supply that rate, so a different request is reported rather
/// than silently ignored.
pub fn default_audio_backend(format: AudioFormat) -> Result<Box<dyn AudioBackend>> {
    #[cfg(windows)]
    {
        if format != AudioFormat::default() {
            tracing::warn!(
                "audio was requested as {:?} but the Windows loopback backend only \
                 produces {:?}",
                format,
                AudioFormat::default()
            );
        }
        Ok(Box::new(crate::wasapi::WasapiLoopback::new()?))
    }
    #[cfg(not(windows))]
    {
        Ok(Box::new(crate::stub::StubAudio::new(format)))
    }
}

/// `CoInitializeEx(COINIT_MULTITHREADED)` for the calling thread, shared by both
/// Windows backends.
///
/// Returns whether *this* call took the apartment, which is exactly when the caller owes
/// a matching `CoUninitialize`. Already-initialised threads (`S_FALSE`) and threads that
/// live in a different apartment (`RPC_E_CHANGED_MODE`) both report `false`: COM is
/// usable either way, and neither case may be uninitialised by us.
#[cfg(windows)]
pub(crate) fn init_com() -> Result<bool> {
    use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    // SAFETY: COM is being initialised for the current thread; a null reserved pointer
    // is the documented value.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr == S_OK {
        return Ok(true);
    }
    if hr == S_FALSE {
        tracing::debug!("COM was already initialised on this thread");
        return Ok(false);
    }
    if hr == RPC_E_CHANGED_MODE {
        tracing::warn!(
            "this thread is already in a single-threaded COM apartment; audio and video \
             capture are expected to work but that is not a tested configuration"
        );
        return Ok(false);
    }
    anyhow::bail!("CoInitializeEx(COINIT_MULTITHREADED) failed: {hr:?}");
}
