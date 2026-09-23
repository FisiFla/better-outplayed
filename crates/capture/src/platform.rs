//! Platform plumbing for the Windows capture backends.
//!
//! Everything Windows-specific that the backends share lives here, so that the
//! `#[cfg(windows)]` surface of this crate stays small and type-checkable from a
//! non-Windows host (`cargo check --target x86_64-pc-windows-msvc` does not link).

#[cfg(windows)]
use anyhow::Result;

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
