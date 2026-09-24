//! What this machine's hardware video encoders are, and whether they will take a GPU texture.
//!
//! Tier 2 rests on one assumption nothing in this tree can check from a development host: that a
//! hardware encoder MFT exists here **and** will accept the D3D11 device the capture backend
//! already owns. That is what makes handing it a [`localplay_capture::Frame`]'s texture possible
//! instead of reading 33.2 MB back into system memory per frame — which is ~97% of what the 4K
//! pipeline does (§14 of `docs/verification-status.md`).
//!
//! So this asks the machine rather than the documentation, deliberately in the shape of
//! `xtask probe`: it starts Media Foundation, enumerates the hardware encoder MFTs for H.264, and
//! runs the actual handshake the encoder will have to run — `MFT_MESSAGE_SET_D3D_MANAGER` with a
//! real device manager over a real D3D11 device of the same kind the capture path creates. **No
//! window is opened, nothing is captured and no input is synthesised**, which matters because the
//! machine this runs on is the one running an anti-cheat.
//!
//! A refusal in that handshake is not a detail: it is the answer to whether this design is
//! viable, and it is far cheaper to learn here than after the encoder is written.

use anyhow::{Context, Result};
use windows::core::{Interface, PWSTR};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFDXGIDeviceManager, IMFTransform, MFCreateDXGIDeviceManager, MFShutdown,
    MFStartup, MFTEnumEx, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_HARDWARE_URL_Attribute, MFT_FRIENDLY_NAME_Attribute,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_REGISTER_TYPE_INFO, MFMediaType_Video, MFSTARTUP_FULL,
    MFVideoFormat_H264, MF_VERSION, MF_TRANSFORM_ASYNC_UNLOCK,
};
use windows::Win32::System::Com::CoTaskMemFree;

/// One hardware video encoder this machine offers, and what it said when asked for the handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardwareEncoder {
    /// The MFT's own name for itself (`MFT_FRIENDLY_NAME_Attribute`).
    pub name: String,
    /// `MFT_ENUM_HARDWARE_URL_Attribute`, the marker that distinguishes a real hardware MFT from
    /// a software one that happens to be enumerable under the hardware flag.
    pub hardware_url: Option<String>,
    /// `MF_TRANSFORM_ASYNC`. An asynchronous MFT refuses to be configured until it is *unlocked*
    /// — measured on the box: NVIDIA's MFT answers an unlocked handshake with
    /// `MF_E_TRANSFORM_ASYNC_LOCKED` (`0xC00D6D77`). [`ask_one`] therefore unlocks before asking,
    /// so `accepts_d3d11` is about D3D11 and not about the async contract.
    pub asynchronous: bool,
    /// **The question this module exists for**: did `MFT_MESSAGE_SET_D3D_MANAGER` succeed?
    pub accepts_d3d11: bool,
    /// Why not, when it did not — a driver's own words are worth more than a summary of them.
    pub refusal: Option<String>,
}

/// Every hardware H.264 encoder on this machine, asked whether it takes a D3D11 device manager.
///
/// Returns an error only when the question could not be *asked* (Media Foundation would not start,
/// the enumeration itself failed). A machine that offers no hardware encoder returns an empty list,
/// because "there are none" is an answer rather than a failure.
pub fn probe_hardware_encoders() -> Result<Vec<HardwareEncoder>> {
    // SAFETY: `MFStartup` is the documented first call before any other MF entry point, and it is
    // paired with `MFShutdown` on every path out of this function.
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup")?;
    let probed = probe_started_encoders();
    // SAFETY: balances the successful `MFStartup` above. A failure here means Media Foundation did
    // not shut down cleanly, which is not a reason to discard an answer already measured.
    unsafe {
        let _ = MFShutdown();
    }
    probed
}

fn probe_started_encoders() -> Result<Vec<HardwareEncoder>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;

    // The type to match is the *input*: which encoders can take video and produce H.264.
    // SAFETY: `activates`/`count` are the out-parameters this call fills, and `input` outlives it.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&input),
            &mut activates,
            &mut count,
        )
    }
    .context("MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE)")?;

    if activates.is_null() || count == 0 {
        // The array is COM-allocated whether or not it holds anything, so a non-null empty one
        // still has to be freed — but "no hardware encoder" is an answer, not an error.
        if !activates.is_null() {
            // SAFETY: `activates` came from `MFTEnumEx`, which allocates with the COM task
            // allocator, and this is the matching free.
            unsafe { CoTaskMemFree(Some(activates as *const std::ffi::c_void)) };
        }
        return Ok(Vec::new());
    }

    // One device and one manager, shared by every candidate: that is exactly what the encoder will
    // do, and a device created per candidate would answer a different question.
    let device = create_capture_kind_device()?;
    let manager = create_device_manager(&device)?;

    // SAFETY: `activates` points at `count` initialised `Option<IMFActivate>`s, which is what
    // `MFTEnumEx` just wrote there.
    let candidates = unsafe { std::slice::from_raw_parts(activates, count as usize) };
    let mut probed = Vec::with_capacity(candidates.len());
    for candidate in candidates.iter().flatten() {
        probed.push(ask_one(candidate, &manager));
    }

    // SAFETY: as above — the array is released here.
    unsafe { CoTaskMemFree(Some(activates as *const std::ffi::c_void)) };
    Ok(probed)
}

/// Ask one enumerated MFT for the handshake, and describe it either way.
fn ask_one(activate: &IMFActivate, manager: &IMFDXGIDeviceManager) -> HardwareEncoder {
    let name = attribute_string(activate, &MFT_FRIENDLY_NAME_Attribute)
        .unwrap_or_else(|| "<unnamed MFT>".to_string());
    let hardware_url = attribute_string(activate, &MFT_ENUM_HARDWARE_URL_Attribute);
    let asynchronous =
        attribute_u32(activate, &windows::Win32::Media::MediaFoundation::MF_TRANSFORM_ASYNC)
            .map(|v| v != 0)
            .unwrap_or(false);

    // SAFETY: `ActivateObject` is this `IMFActivate`'s own method; the transform it returns is
    // shut down at the end of this function.
    let activated: Result<IMFTransform> = unsafe { activate.ActivateObject() }
        .context("IMFActivate::ActivateObject(IMFTransform)");
    let transform = match activated {
        Ok(transform) => transform,
        Err(err) => {
            return HardwareEncoder {
                name,
                hardware_url,
                asynchronous,
                accepts_d3d11: false,
                refusal: Some(format!("could not be activated: {err:#}")),
            }
        }
    };

    // An asynchronous MFT will not be configured until the caller says it understands the async
    // contract, and every hardware encoder on this box is asynchronous. Without this the
    // handshake comes back `MF_E_TRANSFORM_ASYNC_LOCKED` — *"the caller does not appear to support
    // this transform's asynchronous capabilities"* — which reads like a refusal of D3D11 and is
    // not one. That distinction is the whole point of asking: without the unlock the probe would
    // have reported "no hardware encoder takes a GPU texture" on a machine that has one.
    if asynchronous {
        // SAFETY: `GetAttributes` is this transform's own method, and the handle it returns
        // borrows the transform, which is alive here.
        let unlocked = match unsafe { transform.GetAttributes() } {
            Ok(attributes) => {
                // SAFETY: setting an attribute on the transform's own attribute store, which is
                // how an async MFT is unlocked.
                unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
            }
            Err(err) => Err(err),
        };
        if let Err(err) = unlocked {
            return HardwareEncoder {
                name,
                hardware_url,
                asynchronous,
                accepts_d3d11: false,
                refusal: Some(format!("could not be unlocked for asynchronous use: {err}")),
            };
        }
    }

    // The handshake itself. `MFT_MESSAGE_SET_D3D_MANAGER` carries the manager as its *ulParam*,
    // which is why the raw pointer crosses as a `usize` — that is the documented protocol, not a
    // shortcut.
    // SAFETY: `manager` outlives the call, and the parameter is the manager's own interface
    // pointer, which is what the message requires.
    let handshake =
        unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) };
    // Detach the manager before letting the transform go: Media Foundation's contract is that the
    // MFT is told when it no longer holds one (`ulParam = 0`). The transform itself needs no
    // shutdown — it is this function's own activation and its reference drops here — but the
    // device manager outlives it, so leaving the association dangling would be a leak of the
    // association rather than of the object.
    // SAFETY: the transform is live and the parameter is the documented detach value.
    unsafe {
        let _ = transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
    }

    match handshake {
        Ok(()) => HardwareEncoder {
            name,
            hardware_url,
            asynchronous,
            accepts_d3d11: true,
            refusal: None,
        },
        Err(err) => HardwareEncoder {
            name,
            hardware_url,
            asynchronous,
            accepts_d3d11: false,
            refusal: Some(format!("refused MFT_MESSAGE_SET_D3D_MANAGER: {err}")),
        },
    }
}

/// A D3D11 device of the kind the capture backend creates.
///
/// `D3D11_CREATE_DEVICE_BGRA_SUPPORT` is the flag that matters: Windows Graphics Capture requires
/// it, so a device without it could not receive a captured texture and the handshake would be
/// answering a question nobody is asking.
fn create_capture_kind_device() -> Result<ID3D11Device> {
    let mut device: Option<ID3D11Device> = None;
    // SAFETY: `device` is the out-parameter; the adapter and software parameters are null by
    // design (the default hardware adapter), and the feature-level array is empty, which asks for
    // the highest level the adapter supports.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .context("D3D11CreateDevice(HARDWARE, BGRA_SUPPORT)")?;
    device.context("D3D11CreateDevice returned no device")
}

/// A Media Foundation device manager holding `device`.
fn create_device_manager(device: &ID3D11Device) -> Result<IMFDXGIDeviceManager> {
    let mut token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    // SAFETY: both are the out-parameters this call fills.
    unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
        .context("MFCreateDXGIDeviceManager")?;
    let manager = manager.context("MFCreateDXGIDeviceManager returned no manager")?;
    // SAFETY: `device` is a live D3D11 device and `token` is the one this manager was created
    // with, which is what ties the two together.
    unsafe { manager.ResetDevice(device, token) }.context("IMFDXGIDeviceManager::ResetDevice")?;
    Ok(manager)
}

/// An MFT attribute that holds a string, if it is there.
fn attribute_string(activate: &IMFActivate, key: &windows::core::GUID) -> Option<String> {
    let mut value = PWSTR::null();
    let mut length = 0u32;
    // SAFETY: `value`/`length` are the out-parameters; the allocation is freed below on the
    // success path, which is the documented contract for `GetAllocatedString`.
    if unsafe { activate.GetAllocatedString(key, &mut value, &mut length) }.is_err() {
        return None;
    }
    // SAFETY: a non-empty `PWSTR` from `GetAllocatedString` is a NUL-terminated wide string this
    // function owns.
    let text = unsafe { value.to_string() }.ok();
    if !value.is_null() {
        // SAFETY: as above — COM allocated it, so COM frees it.
        unsafe { CoTaskMemFree(Some(value.0 as *const std::ffi::c_void)) };
    }
    text
}

/// An MFT attribute that holds a `u32`, if it is there.
fn attribute_u32(activate: &IMFActivate, key: &windows::core::GUID) -> Option<u32> {
    // SAFETY: `GetUINT32` reads the attribute and returns by value.
    unsafe { activate.GetUINT32(key) }.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What this machine offers, and whether it will take the capture path's own D3D11 device.
    ///
    /// Deliberately a test that **reports** and asserts only what is a fact about the platform
    /// rather than about one GPU: the enumeration must work, and every MFT the hardware flag
    /// returned must either have accepted the handshake or said why not. Requiring an encoder to
    /// *exist* would fail on a machine that legitimately has none, which is not what this is for —
    /// the numbers are the output, and they are printed for the record.
    #[test]
    fn the_hardware_encoders_are_enumerable_and_answer_the_d3d11_handshake() {
        let encoders = probe_hardware_encoders().expect("the machine can be asked");
        eprintln!("hardware H.264 encoder MFTs on this machine: {}", encoders.len());
        for encoder in &encoders {
            eprintln!(
                "  {:<44} d3d11={:<5} async={:<5} url={}",
                encoder.name,
                encoder.accepts_d3d11,
                encoder.asynchronous,
                encoder.hardware_url.as_deref().unwrap_or("-"),
            );
            if let Some(why) = &encoder.refusal {
                eprintln!("      refused: {why}");
            }
        }
        for encoder in &encoders {
            assert!(
                encoder.accepts_d3d11 || encoder.refusal.is_some(),
                "{} neither accepted the handshake nor gave a reason, which would make the \
                 answer unusable either way",
                encoder.name
            );
        }
    }
}
