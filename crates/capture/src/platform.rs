//! Backend selection, so every `#[cfg(windows)]` line lives inside this crate.
//!
//! `cargo check --target x86_64-pc-windows-msvc` type-checks this crate from a non-Windows host
//! (it does not link), the only Windows gate without Windows hardware. Selecting here rather than
//! in the CLI makes that gate cover the choice as well as the backends.
//!
//! # No silent stub fallback on Windows
//!
//! On Windows these functions return a real backend or fail. A synthetic stand-in would make the
//! application look like it is buffering while capturing nothing, surfacing only later as an
//! empty clip. The stubs are selected only off Windows.

use crate::stub::StubConfig;
use crate::target::CaptureTarget;
use crate::{AudioBackend, AudioFormat, CaptureBackend, GameAudioSource, GameFinder};
use anyhow::Result;

/// The video backend for this platform.
///
/// `stub` describes the synthetic source used off Windows. On Windows the real backend reports
/// the monitor's native size through [`CaptureBackend::native_size`], so `stub` is not consulted.
pub fn default_video_backend(
    stub: StubConfig,
    target: &CaptureTarget,
) -> Result<Box<dyn CaptureBackend>> {
    #[cfg(windows)]
    {
        // The stub's size is irrelevant: WGC reports the monitor it will capture and the caller reads it
        // back via `native_size`. A configured `encode.output_size` that differs is scaled, so no warning.
        let _ = stub;
        Ok(Box::new(crate::wgc::WgcCapture::new(target)?))
    }
    #[cfg(not(windows))]
    {
        if !target.is_default() {
            tracing::warn!(
                ?target,
                "monitor/window capture needs Windows Graphics Capture; the synthetic stub \
                 has no screen, so the target is ignored"
            );
        }
        Ok(Box::new(crate::stub::StubCapture::new(stub)))
    }
}

/// The audio backend for this platform.
///
/// `format` is what the synthetic source produces off Windows. On Windows the WASAPI backend
/// always produces `AudioFormat::default()` (48kHz stereo s16le), so a different request is
/// reported rather than silently ignored.
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

/// The game-audio backend: like [`default_audio_backend`], but under [`GameAudioSource::App`] it
/// captures only the process `find_game` reports, where Windows supports that. `output_device` is the
/// output endpoint id whole-output capture listens to (empty: the Windows default).
pub fn game_audio_backend(
    format: AudioFormat,
    source: GameAudioSource,
    find_game: GameFinder,
    output_device: &str,
) -> Result<Box<dyn AudioBackend>> {
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
        Ok(Box::new(crate::wasapi::WasapiLoopback::with_source(source, Some(find_game), output_device)?))
    }
    #[cfg(not(windows))]
    {
        let _ = (source, find_game, output_device);
        Ok(Box::new(crate::stub::StubAudio::new(format)))
    }
}

/// `CoInitializeEx(COINIT_MULTITHREADED)` for the calling thread, shared by both Windows backends.
///
/// Returns whether this call took the apartment, i.e. when the caller owes a matching
/// `CoUninitialize`. Already-initialised threads (`S_FALSE`) and threads in a different apartment
/// (`RPC_E_CHANGED_MODE`) report `false`: COM is usable either way and we must not uninitialise it.
#[cfg(windows)]
pub(crate) fn init_com() -> Result<bool> {
    use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    // SAFETY: COM is being initialised for the current thread; a null reserved pointer is the documented value.
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
