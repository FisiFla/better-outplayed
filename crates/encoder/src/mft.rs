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

use crate::ffmpeg::MFT_TIMEOUT;
use anyhow::{bail, Context, Result};
use localplay_capture::GpuTexture;
use std::time::{Duration, Instant};
use windows::core::{Interface, PWSTR};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFTransform,
    MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateSample,
    MFShutdown, MFStartup, MFTEnumEx, MFT_CATEGORY_VIDEO_ENCODER, MFT_CATEGORY_VIDEO_PROCESSOR,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_HARDWARE_URL_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_SET_D3D_MANAGER, MFT_REGISTER_TYPE_INFO,
    MFMediaType_Video, MFSTARTUP_FULL, MFVideoFormat_ARGB32, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoFormat_P010, MFVideoFormat_RGB32, MFVideoFormat_YUY2,
    eAVEncH264VLevel5_1, eAVEncH264VProfile_High, MFVideoInterlace_Progressive, MF_VERSION,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_LEVEL, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK, METransformHaveOutput,
    METransformNeedInput, MF_EVENT_FLAG_NO_WAIT, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MF_MT_MAX_KEYFRAME_SPACING,
};

/// What an encoder asked for, when its events were drained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    /// `METransformNeedInput`: it will take a frame now.
    Input,
    /// Nothing queued — which is what a poll looks like when there is no work.
    Quiet,
}
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
    /// The input subtypes this MFT will accept, named where this build knows the name.
    ///
    /// This decides the *shape* of the chain rather than whether it exists. Windows Graphics
    /// Capture delivers **BGRA8**; if the encoder will only take **NV12**, then something has to
    /// convert between them, and the honest place for that is a Video Processor MFT on the GPU
    /// rather than a CPU loop in this process — which is precisely the cost this whole plan exists
    /// to remove. If it accepts BGRA directly, the texture can go straight in.
    /// Empty when the MFT could not be asked — it would not activate, or it would not unlock —
    /// which is what the `refusal` beside it explains.
    pub input_subtypes: Vec<String>,
    /// Which of the formats this pipeline might feed it the MFT actually **took**, by being asked
    /// to accept one.
    ///
    /// This is the answer, where `input_subtypes` above is only the enumeration attempt: a
    /// hardware encoder refuses to enumerate its types (`GetInputAvailableType` came back empty on
    /// the box even after the D3D11 handshake), but it will not refuse a `SetInputType` it likes —
    /// and that is the same call the encoder makes, so agreement here is agreement about the real
    /// thing. Names come from [`CANDIDATE_INPUTS`].
    pub accepts: Vec<String>,
    /// Why no full answer could be had, when that happened — a driver's own words are worth more
    /// than a summary of them. Covers the handshake, an activation that failed, an unlock that was
    /// refused, and a type negotiation that would not start.
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
                input_subtypes: Vec::new(),
                accepts: Vec::new(),
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
                input_subtypes: Vec::new(),
                accepts: Vec::new(),
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

    let input_subtypes = input_subtypes(&transform);
    // Asked at a modest size: the answer is about the *format*, and a 640x480 type is as much a
    // statement about that as a 3840x2160 one, without depending on this machine's display.
    // A negotiation failure is a finding rather than a panic: it means the machine will not tell us
    // what it eats, which is exactly what `refusal` is for.
    let (accepts, negotiation) = match accepted_subtypes(&transform, (640, 480)) {
        Ok(accepts) => (accepts, None),
        Err(err) => (Vec::new(), Some(format!("{err:#}"))),
    };
    // The transform is released at the end of this function; the attribute query above is the last
    // thing it is needed for.
    match handshake {
        Ok(()) => HardwareEncoder {
            name,
            hardware_url,
            asynchronous,
            accepts_d3d11: true,
            input_subtypes,
            accepts,
            refusal: negotiation,
        },
        Err(err) => HardwareEncoder {
            name,
            hardware_url,
            asynchronous,
            accepts_d3d11: false,
            input_subtypes,
            accepts,
            refusal: Some(match negotiation {
                Some(note) => format!("refused MFT_MESSAGE_SET_D3D_MANAGER: {err}; and {note}"),
                None => format!("refused MFT_MESSAGE_SET_D3D_MANAGER: {err}"),
            }),
        },
    }
}

/// The frame rate this probe negotiates, in **both** directions.
///
/// One constant and not two, because two of them is what the first version of this had and the two
/// disagreed — an output type at 30 fps and an input type at 60 — which Media Foundation refused
/// with `MF_E_INVALIDMEDIATYPE`, whose own text says *"invalid, inconsistent, or not supported"*.
/// The word doing the work there was "inconsistent": nothing was wrong with either rate on its own.
/// 30 rather than 60 because 4K at 60 needs H.264 level 5.2 while 4K at 30 fits 5.1, and this
/// negotiates 5.1 (`eAVEncH264VLevel5_1`) — asking for a rate the declared level cannot carry is
/// the other way to be refused by a type that is perfectly well formed.
const FRAME_RATE: (u32, u32) = (30, 1);

/// The formats worth asking about, in the order worth asking.
///
/// `RGB32` and `ARGB32` first because Windows Graphics Capture delivers BGRA8 and either of those
/// names means the texture can go straight in, with no conversion anywhere. `NV12` last because it
/// is the format a hardware encoder most often *wants* and the one this pipeline cannot produce
/// without help — if only it is accepted, the chain needs a Video Processor MFT on the GPU.
const CANDIDATE_INPUTS: [(&windows::core::GUID, &str); 3] = [
    (&MFVideoFormat_RGB32, "RGB32 (BGRA byte order — what WGC delivers)"),
    (&MFVideoFormat_ARGB32, "ARGB32"),
    (&MFVideoFormat_NV12, "NV12"),
];

/// Which candidate input formats this transform will actually take.
///
/// Asked with `SetInputType` rather than read from an enumeration, because an asynchronous
/// hardware MFT will not enumerate (measured on the box) and because *accepting a type* is the fact
/// that matters: it is the call the encoder will make, with a media type built the same way.
fn accepted_subtypes(transform: &IMFTransform, size: (u32, u32)) -> Result<Vec<String>> {
    // **The output type first, and this is not a detail.** An encoder will not accept an *input*
    // type until it knows what it is producing: Media Foundation negotiates the output side first,
    // and an input attempt made before that is refused for the ordering rather than for the format.
    // Measured the hard way — the first version of this probe asked for the input alone and
    // reported "takes no candidate input format" on an encoder that has one, which is the same
    // shape of false negative as the async lock was, and would have been believed just as easily.
    set_h264_output_type(transform, size)
        .context("setting the output type, which must come before the input")?;

    let mut accepted = Vec::new();
    let mut refusals = Vec::new();
    for (subtype, name) in CANDIDATE_INPUTS {
        match accepts_subtype(transform, subtype, size) {
            Ok(()) => accepted.push(name.to_string()),
            Err(err) => refusals.push(format!("{name}: {err:#}")),
        }
    }
    if accepted.is_empty() {
        // Every candidate refused, which is worth saying in full: the driver's own words for each
        // are the only thing that can distinguish "wrong format" from "wrong ordering" from
        // "wrong size".
        bail!("set an output type but no candidate input type was accepted: {}", refusals.join("; "));
    }
    Ok(accepted)
}

/// Configure the encoder's output: H.264 at `size`, which is what makes it willing to talk about
/// its input at all.
fn set_h264_output_type(transform: &IMFTransform, size: (u32, u32)) -> Result<()> {
    // SAFETY: `MFCreateMediaType` returns an empty type this function owns.
    let media_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType")?;
    let pair = |first: u32, second: u32| ((first as u64) << 32) | second as u64;
    // SAFETY: every call sets an attribute on a media type this function owns.
    unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pair(size.0, size.1))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pair(FRAME_RATE.0, FRAME_RATE.1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pair(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        // A bitrate is not optional for a rate-controlled encoder, and this one is the project's own
        // configured 20 Mbit/s rather than a token value: measured, a token 2 Mbit/s at 4K is not
        // enough for the encoder to accept the size at all.
        media_type.SetUINT32(&MF_MT_AVG_BITRATE, 20_000_000)?;
        // **The profile and the level are the load-bearing attributes at 4K.** Without them this MFT
        // refuses *every* input format at 3840x2160 — including NV12, which it encodes for ffmpeg
        // every day — because the level it would have to claim to carry 4K@60 is not one it can
        // assume on the caller's behalf. Measured: 0xC00D36B4 for ARGB32 and NV12, 0xC00D36BD for
        // RGB32, all three before these two lines existed.
        media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
        media_type.SetUINT32(&MF_MT_MPEG2_LEVEL, eAVEncH264VLevel5_1.0 as u32)?;
        // **The GOP is the MFT's to set, and this is where it is set.**
        //
        // The plan named this as the risk that lives in this half of the change: `-force_key_frames`
        // cannot apply to a stream ffmpeg is only copying, so nothing downstream can impose the
        // segment boundaries the replay path depends on. It has to come from the encoder, and a
        // keyframe a second — the segment length — is what makes one fragment per segment, which is
        // what `MemoryRingBuffer` parses and what the lossless clip path cuts on.
        media_type.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, FRAME_RATE.0)?;
    }
    // SAFETY: setting the output type on a transform that was just unlocked and given its device
    // manager, with a media type that outlives the call.
    unsafe { transform.SetOutputType(0, &media_type, 0) }.context("SetOutputType(H264)")
}

/// Try to set `subtype` as the transform's input type at `size`.
///
/// The media type carries the four attributes every video encoder insists on seeing — major type,
/// subtype, frame size, frame rate, interlace mode and pixel aspect ratio — packed the way Media
/// Foundation packs them (two 32-bit halves in a `u64`), because a type missing any of them is
/// rejected for the missing attribute rather than for the format under test.
fn accepts_subtype(
    transform: &IMFTransform,
    subtype: &windows::core::GUID,
    size: (u32, u32),
) -> Result<()> {
    // SAFETY: `MFCreateMediaType` is the documented constructor and returns an empty type.
    let media_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType")?;
    let pair = |first: u32, second: u32| ((first as u64) << 32) | second as u64;
    // SAFETY: every call below sets an attribute on a media type this function owns.
    unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pair(size.0, size.1))?;
        // The same rate as the output type, from the same constant: a probe that guessed a
        // different one here would be measuring its own inconsistency.
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pair(FRAME_RATE.0, FRAME_RATE.1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pair(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    }
    // SAFETY: setting the input type on a transform that was just unlocked and given its device
    // manager, with a media type that outlives the call.
    unsafe { transform.SetInputType(0, &media_type, 0) }
        .with_context(|| format!("SetInputType({subtype:?})"))
}

/// The input subtypes an MFT offers, by enumerating until it stops answering.
///
/// Bounded rather than unbounded: an MFT that answered forever would hang the probe, and no
/// encoder has ever offered more than a handful. The documented signal for "no more types" is an
/// error, which is the break.
fn input_subtypes(transform: &IMFTransform) -> Vec<String> {
    const ENOUGH: u32 = 32;
    let mut found = Vec::new();
    for index in 0..ENOUGH {
        // SAFETY: enumerating the first input stream's available types. The index is walked from
        // zero until the call refuses, which is the documented terminator.
        let Ok(media_type) = (unsafe { transform.GetInputAvailableType(0, index) }) else { break };
        // SAFETY: reading the subtype GUID out of a media type this call just returned.
        match unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) } {
            Ok(subtype) => found.push(describe_subtype(&subtype)),
            Err(_) => break,
        }
    }
    found
}

/// A video subtype's name, for the ones this build knows, and its GUID otherwise.
///
/// `RGB32` is included deliberately: Media Foundation's name for it is BGRX/RGB32 in memory, which
/// is the *same byte order* Windows Graphics Capture delivers as BGRA8, so an encoder that lists it
/// can take a captured texture without a colour conversion — which is not obvious from the name.
fn describe_subtype(guid: &windows::core::GUID) -> String {
    let known: [(&windows::core::GUID, &str); 6] = [
        (&MFVideoFormat_NV12, "NV12"),
        (&MFVideoFormat_ARGB32, "ARGB32"),
        (&MFVideoFormat_RGB32, "RGB32 (BGRA byte order — what WGC delivers)"),
        (&MFVideoFormat_YUY2, "YUY2"),
        (&MFVideoFormat_P010, "P010"),
        (&MFVideoFormat_H264, "H264"),
    ];
    for (candidate, name) in known {
        if candidate == guid {
            return name.to_string();
        }
    }
    format!("{guid:?}")
}

/// Whether this machine offers a hardware-accelerated **Video Processor** MFT.
///
/// Only needed if the encoder refuses BGRA: a processor is where a BGRA→NV12 conversion belongs if
/// one is required, because it runs on the GPU. Reported either way, since "there is one" is worth
/// knowing before the design depends on it.
pub fn probe_video_processors() -> Result<Vec<String>> {
    enumerate_names(MFT_CATEGORY_VIDEO_PROCESSOR)
}

/// The friendly names of the MFTs in one category, hardware-flagged and sorted.
fn enumerate_names(category: windows::core::GUID) -> Result<Vec<String>> {
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: `activates`/`count` are the out-parameters this call fills.
    unsafe {
        MFTEnumEx(
            category,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            None,
            &mut activates,
            &mut count,
        )
    }
    .context("MFTEnumEx for a category")?;
    if activates.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: `activates` points at `count` initialised `Option<IMFActivate>`s written by the call
    // above, and the array is released with the matching free below.
    let candidates = unsafe { std::slice::from_raw_parts(activates, count as usize) };
    let names = candidates
        .iter()
        .flatten()
        .map(|candidate| {
            attribute_string(candidate, &MFT_FRIENDLY_NAME_Attribute)
                .unwrap_or_else(|| "<unnamed MFT>".to_string())
        })
        .collect();
    // SAFETY: as above.
    unsafe { CoTaskMemFree(Some(activates as *const std::ffi::c_void)) };
    Ok(names)
}

/// A D3D11 device of the kind the capture backend creates.
///
/// `D3D11_CREATE_DEVICE_BGRA_SUPPORT` is the flag that matters: Windows Graphics Capture requires
/// it, so a device without it could not receive a captured texture and the handshake would be
/// answering a question nobody is asking.
/// A D3D11 device of the kind the capture backend creates.
///
/// Public because the probe needs one: in the real pipeline the device is capture's, and that is why
/// [`MftEncoder::open`] takes one instead of making its own.
pub fn create_capture_kind_device() -> Result<ID3D11Device> {
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

/// A hardware encoder MFT, configured for a size and fed one GPU texture at a time.
///
/// This is the Tier 2 encoder core, and its whole point is that [`MftEncoder::push_texture`] moves
/// **no pixels through the CPU**: the captured texture goes to the encoder as a DXGI surface buffer
/// on the GPU it already lives on. Measured context: the readback this replaces is ~97% of what the
/// 4K pipeline does (§14 of `docs/verification-status.md`).
///
/// Three things about opening it are measured rather than assumed, each of which reported a
/// confident false negative when done the obvious way — see the module note and the plan:
///
/// * an asynchronous MFT must be **unlocked** before it can be configured;
/// * the **output type must be set before the input**, because an encoder will not negotiate an
///   input until it knows what it is producing;
/// * the encoder must be **chosen**, not assumed: the hardware flag enumerates MFTs that cannot be
///   instantiated on this machine at all.
pub struct MftEncoder {
    transform: IMFTransform,
    /// The same transform seen as an event generator, which is how an asynchronous MFT says it
    /// wants input and has output. Cast once at open rather than per frame.
    events: IMFMediaEventGenerator,
    /// Held because the manager only borrows it: releasing the device while an MFT holds a manager
    /// over it is exactly the dangling-association case Media Foundation warns about.
    device: ID3D11Device,
    size: (u32, u32),
    /// Which MFT was selected, for the log and for the record.
    pub encoder_name: String,
    /// Which input format it actually took, at this size.
    ///
    /// Reported because it decides whether the chain needs anything else in it: `ARGB32` is what
    /// Windows Graphics Capture delivers, so taking it means the captured texture goes straight in,
    /// while taking only `NV12` means something has to convert — and that something has to be on the
    /// GPU or this whole exercise has moved the copy rather than removed it.
    pub input_format: String,
}

// SAFETY: the members are COM interfaces the `windows` crate marks `Send`+`Sync` for the ones that
// are (a D3D11 device and an MFT are both usable from one thread at a time), and this type is moved
// to the encoder's own thread and used there, never concurrently. The device is additionally
// expected to be multithread-protected by its owner once capture and this encoder both touch it.
unsafe impl Send for MftEncoder {}

impl MftEncoder {
    /// Open a hardware H.264 encoder on `device` for frames of `size`, taking `ARGB32` input.
    ///
    /// `device` is the **capture path's** device, deliberately: an MFT given a manager over a
    /// different device cannot be handed the captured texture at all, so the two have to be the
    /// same one. That is why this takes a device rather than creating its own.
    pub fn open(device: &ID3D11Device, size: (u32, u32)) -> Result<Self> {
        // SAFETY: `MFStartup` is refcounted and its pair is in `Drop`, so opening several encoders
        // in one process is a supported sequence.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup")?;
        match Self::open_started(device, size) {
            Ok(encoder) => Ok(encoder),
            Err(err) => {
                // SAFETY: balances the `MFStartup` above, which succeeded.
                unsafe {
                    let _ = MFShutdown();
                }
                Err(err)
            }
        }
    }

    fn open_started(device: &ID3D11Device, size: (u32, u32)) -> Result<Self> {
        let manager = create_device_manager(device)?;
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        // SAFETY: `activates`/`count` are the out-parameters this call fills; `input` outlives it.
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
        .context("MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, hardware)")?;
        if activates.is_null() || count == 0 {
            if !activates.is_null() {
                // SAFETY: COM-allocated by `MFTEnumEx`; the matching free.
                unsafe { CoTaskMemFree(Some(activates as *const std::ffi::c_void)) };
            }
            bail!("no hardware H.264 encoder MFT on this machine");
        }
        // SAFETY: `activates` points at `count` initialised entries written by the call above.
        let candidates = unsafe { std::slice::from_raw_parts(activates, count as usize) };

        let mut refusals = Vec::new();
        let mut chosen = None;
        for candidate in candidates.iter().flatten() {
            let name = attribute_string(candidate, &MFT_FRIENDLY_NAME_Attribute)
                .unwrap_or_else(|| "<unnamed MFT>".to_string());
            match configure_encoder(candidate, &manager, size) {
                Ok((transform, input_format)) => {
                    chosen = Some((transform, name, input_format));
                    break;
                }
                // Not fatal: the hardware flag enumerates encoders this machine cannot instantiate,
                // which is a fact about the machine rather than a failure of the search.
                Err(err) => refusals.push(format!("{name}: {err:#}")),
            }
        }
        // SAFETY: as above.
        unsafe { CoTaskMemFree(Some(activates as *const std::ffi::c_void)) };

        let (transform, encoder_name, input_format) = chosen.ok_or_else(|| {
            anyhow::anyhow!(
                "no hardware encoder here could be configured for {size:?}: {}",
                refusals.join("; ")
            )
        })?;
        // SAFETY: an asynchronous MFT implements the event generator, and this one has just been
        // configured as asynchronous.
        let events: IMFMediaEventGenerator =
            transform.cast().context("IMFTransform as IMFMediaEventGenerator")?;
        Ok(Self {
            transform,
            events,
            device: device.clone(),
            size,
            encoder_name,
            input_format,
        })
    }

    /// Open an encoder for frames shaped like `texture`, taking its device from the texture itself.
    ///
    /// **The device is the texture's by construction, and that is stronger than a promise.** An MFT
    /// can only be handed a texture that lives on the device its manager was built over, so a design
    /// that had the caller pass a device separately would be trusting two components to agree about
    /// which one capture chose — and the failure mode of that disagreement is a texture the encoder
    /// silently cannot see. Asking the texture removes the question: whatever produced this frame is
    /// what the encoder is opened on.
    ///
    /// The geometry comes from the texture for the same reason. It is the size that will actually
    /// arrive rather than the size somebody configured, and for this mode they are required to be the
    /// same anyway — `VideoInput::EncodedBitstream` refuses a scaled output at spawn.
    pub fn open_for_texture(texture: &ID3D11Texture2D) -> Result<Self> {
        // SAFETY: `texture` is a live D3D11 texture and `GetDevice` is the documented way to ask
        // which device it belongs to. The device it returns is a new reference, held by the encoder.
        let device = unsafe { texture.GetDevice() }.context("ID3D11Texture2D::GetDevice")?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `desc` is a valid out-parameter for this texture.
        unsafe { texture.GetDesc(&mut desc) };
        Self::open(&device, (desc.Width, desc.Height))
    }

    /// The geometry this encoder was opened for.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The device this encoder was opened on, which is the capture path's.
    ///
    /// Exposed because it is the device a texture handed to [`MftEncoder::push_texture`] has to
    /// belong to — one the encoder is not managing cannot be wrapped as a media buffer for it. The
    /// probe uses it to build a synthetic frame; the recorder will have got it from capture.
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// Start the stream. Media Foundation's asynchronous contract begins here, not with the first
    /// frame: an async MFT will not accept input until it has been told streaming has begun.
    pub fn start(&mut self) -> Result<()> {
        // SAFETY: both are messages to the encoder's own transform, with no parameter.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0) }
            .context("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING")?;
        // SAFETY: as above.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0) }
            .context("MFT_MESSAGE_NOTIFY_START_OF_STREAM")
    }

    /// Hand the encoder one captured texture once it asks for one, and return what it produced.
    ///
    /// The async contract is the whole reason this is not just "push the frame": an async MFT takes
    /// input only when it has said it wants some (`METransformNeedInput`), and it says so through
    /// the event generator rather than by accepting whatever arrives. `ProcessInput` called at any
    /// other moment answers `MF_E_NOTACCEPTING` — measured, on the first attempt to feed this
    /// encoder — which is why the wait is here rather than at the call site.
    ///
    /// `deadline` bounds the wait for that request: an encoder that never asks is a fault to report,
    /// not a hang to sit in.
    pub fn encode_texture(
        &mut self,
        texture: &ID3D11Texture2D,
        pts_ms: i64,
        deadline: Duration,
    ) -> Result<Vec<u8>> {
        let mut produced = Vec::new();
        let started = Instant::now();
        loop {
            if self.pump_events(&mut produced)? == Asked::Input {
                break;
            }
            if started.elapsed() > deadline {
                bail!(
                    "the encoder did not ask for input within {deadline:?}, so nothing could be                      handed to it"
                );
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        self.push_texture(texture, pts_ms)?;
        // Collect without waiting further: what this frame produced, if anything, is already queued
        // by the time it was accepted.
        let _ = self.pump_events(&mut produced);
        Ok(produced)
    }

    /// Tell the encoder no more input is coming, and take everything it still has.
    ///
    /// A drain is what makes the tail of a stream real: without it the last few frames sit inside
    /// the encoder and never reach the container.
    pub fn finish(&mut self, deadline: Duration) -> Result<Vec<u8>> {
        // SAFETY: a message to the encoder's own transform, with no parameter.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }
            .context("MFT_MESSAGE_COMMAND_DRAIN")?;
        let mut produced = Vec::new();
        let started = Instant::now();
        loop {
            self.pump_events(&mut produced)?;
            if started.elapsed() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        // SAFETY: as above — the stream is over.
        let _ = unsafe { self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0) };
        Ok(produced)
    }

    /// Drain whatever the encoder has to say, collecting its H.264 as it goes.
    fn pump_events(&mut self, produced: &mut Vec<u8>) -> Result<Asked> {
        loop {
            // SAFETY: the event generator on this encoder's own transform. `NO_WAIT` makes an empty
            // queue an error rather than a block, which is what a poll needs.
            let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                // Nothing queued. Not a failure: this is how a poll ends.
                Err(_) => return Ok(Asked::Quiet),
            };
            // SAFETY: reading this event's own type.
            let kind = unsafe { event.GetType() }.context("IMFMediaEvent::GetType")?;
            if kind == METransformNeedInput.0 as u32 {
                return Ok(Asked::Input);
            }
            if kind == METransformHaveOutput.0 as u32 {
                self.take_output(produced)?;
            }
            // Anything else — a drain completing, an error event — is not this exchange's business;
            // the loop simply continues until the queue is empty.
        }
    }

    /// Take one finished sample's bytes out of the encoder.
    fn take_output(&mut self, produced: &mut Vec<u8>) -> Result<()> {
        // `ManuallyDrop` because that is how this struct is generated: the sample inside is owned by
        // the caller once taken, and leaving the field un-taken must not release it twice.
        let mut buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        // SAFETY: one output stream, and the sample is the MFT's to allocate — an encoder that
        // provides samples is the case here, which is why `dwFlags` is zero and the buffer carries
        // no sample of ours. The slice is exactly one element, which is what this stream has, and
        // `status` is the documented out-parameter for the per-stream flags.
        let taken =
            unsafe { self.transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
        // The MFT hands back a sample only when it has one; being asked before that is the
        // documented `MF_E_TRANSFORM_NEED_MORE_INPUT`, which is not an error to propagate.
        match taken {
            Ok(()) => {}
            Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
            Err(err) => return Err(err).context("IMFTransform::ProcessOutput"),
        }
        // SAFETY: taken exactly once, which is the contract for a `ManuallyDrop` field the callee
        // filled in.
        let Some(sample) = (unsafe { std::mem::ManuallyDrop::take(&mut buffer.pSample) }) else {
            return Ok(());
        };
        // SAFETY: this sample is the one the encoder just produced, and the contiguous buffer is a
        // view of it that is locked and unlocked around the copy.
        let bytes = unsafe { sample.ConvertToContiguousBuffer() }.context("ConvertToContiguousBuffer")?;
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut length = 0u32;
        // SAFETY: `data`/`length` are the out-parameters; the buffer is unlocked on every path below.
        unsafe { bytes.Lock(&mut data, None, Some(&mut length)) }.context("IMFMediaBuffer::Lock")?;
        if !data.is_null() && length > 0 {
            // SAFETY: `Lock` guarantees `length` readable bytes at `data` until `Unlock`.
            produced.extend_from_slice(unsafe { std::slice::from_raw_parts(data, length as usize) });
        }
        // SAFETY: pairs the `Lock` above.
        unsafe {
            let _ = bytes.Unlock();
        };
        Ok(())
    }

    /// Hand one captured texture to the encoder, with no copy of its pixels.
    ///
    /// `pts_ms` is media time, in the same milliseconds the rest of the pipeline uses; the encoder
    /// wants 100-nanosecond units, so the conversion happens here rather than at every call site.
    pub fn push_texture(&mut self, texture: &ID3D11Texture2D, pts_ms: i64) -> Result<()> {
        // The documented way to wrap a DXGI surface as a media buffer. `fbottomupwhenlinear` is
        // false because a captured texture is top-down, which is what the encoder expects.
        // SAFETY: `texture` is a live D3D11 texture on this encoder's device, and the IID names the
        // interface it is being wrapped as.
        let buffer = unsafe {
            MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)
        }
        .context("MFCreateDXGISurfaceBuffer")?;
        // SAFETY: `MFCreateSample` returns an empty sample this function owns.
        let sample = unsafe { MFCreateSample() }.context("MFCreateSample")?;
        // SAFETY: both calls act on that sample, and the buffer is referenced by it.
        unsafe { sample.AddBuffer(&buffer) }.context("IMFSample::AddBuffer")?;
        // SAFETY: setting the sample's timestamps; 100ns units are what Media Foundation counts in.
        unsafe {
            sample.SetSampleTime(pts_ms.saturating_mul(10_000))?;
            sample.SetSampleDuration(10_000_000 / 60)?;
        }
        // SAFETY: the encoder's own input stream, with a sample that outlives the call.
        unsafe { self.transform.ProcessInput(0, &sample, 0) }.context("IMFTransform::ProcessInput")
    }
}

impl Drop for MftEncoder {
    fn drop(&mut self) {
        // Detach the device manager before the transform goes, as Media Foundation's contract
        // requires; the transform's own reference then drops with this struct.
        // SAFETY: the transform is live, and the parameter is the documented detach value.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
        }
        // SAFETY: refcounted, balancing the `MFStartup` in `open`.
        unsafe {
            let _ = MFShutdown();
        }
    }
}

/// Configure one candidate: unlock it, give it the device, set its output type, then its input.
///
/// The order is the whole of this function's value. Every step below was learned from a refusal:
/// an async MFT that is not unlocked will not be configured at all, and an encoder asked for an
/// input type before an output type refuses — which reads as "this format is unsupported" and is
/// not that.
fn configure_encoder(
    activate: &IMFActivate,
    manager: &IMFDXGIDeviceManager,
    size: (u32, u32),
) -> Result<(IMFTransform, String)> {
    // SAFETY: this `IMFActivate`'s own method; the transform it returns is returned to the caller.
    let transform: IMFTransform =
        unsafe { activate.ActivateObject() }.context("IMFActivate::ActivateObject(IMFTransform)")?;

    if attribute_u32(activate, &windows::Win32::Media::MediaFoundation::MF_TRANSFORM_ASYNC)
        .map(|v| v != 0)
        .unwrap_or(false)
    {
        // SAFETY: reading the transform's own attribute store, then setting the unlock in it.
        let attributes = unsafe { transform.GetAttributes() }.context("IMFTransform::GetAttributes")?;
        unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
            .context("unlocking an asynchronous MFT")?;
    }

    // SAFETY: the manager outlives the call, and its interface pointer is what the message wants.
    unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
        .context("MFT_MESSAGE_SET_D3D_MANAGER")?;

    set_h264_output_type(&transform, size)?;

    // **Negotiated, not assumed.** Measured: this MFT takes `ARGB32` at 640x480 and refuses it at
    // 3840x2160 with `MF_E_INVALIDMEDIATYPE`, so what it will eat is a function of the size as well
    // as the format. Trying the candidates in order — the *direct* one first, because taking
    // `ARGB32` means the captured texture goes straight in — is what makes this work at whatever
    // size it is asked for rather than at the size someone happened to test.
    let mut refusals = Vec::new();
    for (subtype, name) in CANDIDATE_INPUTS {
        let input = video_type(subtype, size)?;
        // SAFETY: setting the input type on a configured transform, with a media type that outlives
        // the call. A refusal here is expected for some formats at some sizes, not a failure.
        match unsafe { transform.SetInputType(0, &input, 0) } {
            Ok(()) => return Ok((transform, name.to_string())),
            Err(err) => refusals.push(format!("{name}: {err}")),
        }
    }
    bail!(
        "no input format it would take at {size:?}, having set an output type first: {}",
        refusals.join("; ")
    )
}

/// A video media type for one subtype and size, with the attributes every encoder insists on.
///
/// The frame size, frame rate, pixel aspect ratio and interlace mode are packed the way Media
/// Foundation packs them — two 32-bit halves in a `u64` — and a type missing any of them is
/// rejected for the missing attribute rather than for the format under test.
fn video_type(subtype: &windows::core::GUID, size: (u32, u32)) -> Result<IMFMediaType> {
    // SAFETY: `MFCreateMediaType` returns an empty type this function owns.
    let media_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType")?;
    let pair = |first: u32, second: u32| ((first as u64) << 32) | second as u64;
    // SAFETY: every call sets an attribute on that type.
    unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pair(size.0, size.1))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pair(FRAME_RATE.0, FRAME_RATE.1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pair(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    }
    Ok(media_type)
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


/// How many textures may be waiting for the hardware encoder.
///
/// **Two, and it is derived rather than chosen.** Capture hands out each of its three handover
/// textures in turn, so the texture given to frame *n* is blitted into again at frame *n + 3*. A
/// queue of two means the encoder is never more than two frames behind what it was handed, so by
/// the time capture wants that texture back, the encoder is done with it.
///
/// Deepening it would not buy throughput, because the encoder is slower than 60 Hz: the queue fills
/// and frames are dropped either way. What it would buy is tearing — a texture blitted into while
/// the encoder is still reading it, which is a corruption that would show up as a torn picture
/// rather than as an error. Half the ring is what keeps the guarantee true instead of likely.
const QUEUE_DEPTH: usize = 2;

/// The hardware encoder on its own thread, fed through a queue that never blocks.
///
/// `submit` is called from the capture pump, and the encoder is a Media Foundation transform whose
/// synchronous calls take ~30 ms at 4K. Running it inline — which is what this did first — makes the
/// pump wait for it, and the pump is what paces capture: measured, that turned a 56.9 fps recording
/// into 35.0 and dropped 2288 frames of 5700, while the copy it removed was worth 70% of the loop.
/// So the encoder gets a thread of its own and the pump only hands over textures.
///
/// What crosses the boundary is a texture we own, not a frame of pixels: no copy happens here, and
/// nothing is read back.
pub struct MftFeed {
    /// `None` once shut down. Dropping it is the shutdown signal — see [`MftFeed::shutdown`].
    tx: Option<std::sync::mpsc::SyncSender<GpuTexture>>,
    handle: Option<std::thread::JoinHandle<()>>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl MftFeed {
    /// Start the encoder's thread, whose output goes into `out` — the same video queue the pump
    /// used to write to, so the depth accounting stays in one place.
    pub fn spawn(out: std::sync::mpsc::SyncSender<Vec<u8>>) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE_DEPTH);
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter = std::sync::Arc::clone(&dropped);
        let handle = std::thread::Builder::new()
            .name("localplay-mft".to_string())
            .spawn(move || encode_loop(rx, out, counter))
            .expect("spawning the hardware encoder thread");
        Self {
            tx: Some(tx),
            handle: Some(handle),
            dropped,
        }
    }

    /// Hand a frame to the encoder. Never blocks, and never queues deeper than [`QUEUE_DEPTH`]: a
    /// full queue drops the frame and counts it, which is the same bargain the video queue strikes
    /// when it cannot keep up with ffmpeg.
    pub fn submit(&self, texture: GpuTexture) {
        let Some(tx) = self.tx.as_ref() else { return };
        match tx.try_send(texture) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            // The thread is gone, which its own log line explains; the frame is simply not encoded.
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
        }
    }

    /// Frames that did not fit the queue and were never encoded.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Close the queue and wait for the thread to encode what is already in it, then drain.
    ///
    /// **Dropping the sender is the shutdown signal, rather than a message sent down the queue**,
    /// because a message can be refused by a full queue and a shutdown that can be refused is a
    /// hang. Closing the sender ends the `recv` loop after the frames already queued have been
    /// taken, so nothing in flight is lost.
    pub fn shutdown(&mut self) {
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() {
                tracing::warn!("the hardware encoder thread panicked");
            }
        }
    }
}

/// The encoder thread's body.
fn encode_loop(
    rx: std::sync::mpsc::Receiver<GpuTexture>,
    out: std::sync::mpsc::SyncSender<Vec<u8>>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    // Opened on the first texture rather than up front, because both the device and the size come
    // from that texture — and opening it here rather than on the pump takes the open off the
    // capture path as well.
    let mut encoder: Option<MftEncoder> = None;
    let mut pts_ms: i64 = 0;

    while let Ok(texture) = rx.recv() {
        if encoder.is_none() {
            match MftEncoder::open_for_texture(texture.as_raw()) {
                Ok(mut opened) => {
                    if let Err(err) = opened.start() {
                        tracing::error!("could not start the hardware encoder: {err:#}");
                        break;
                    }
                    tracing::info!(
                        encoder = %opened.encoder_name,
                        format = %opened.input_format,
                        size = ?opened.size(),
                        "the hardware encoder is open on its own thread"
                    );
                    encoder = Some(opened);
                }
                Err(err) => {
                    // Counted as a drop so the reporting stays honest about frames that reached this
                    // stage and did not become video.
                    tracing::error!("could not open the hardware encoder: {err:#}");
                    dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    continue;
                }
            }
        }
        let mft = encoder.as_mut().expect("just opened");
        // Media time is counted per frame handed over rather than taken from the caller's clock:
        // what the encoder is being asked to produce is a stream at the configured frame rate, and
        // the timestamps have to march at that rate whether or not frames were dropped on the way
        // here. `MftEncoder::push_texture` converts to the 100-nanosecond units Media Foundation
        // wants.
        pts_ms += (1000 / FRAME_RATE.0 as i64).max(1);
        match mft.encode_texture(texture.as_raw(), pts_ms, MFT_TIMEOUT) {
            Ok(units) if units.is_empty() => {}
            Ok(units) => {
                // Blocking, unlike the handover above: this queue is drained by a thread whose job
                // is to write to ffmpeg, and the pump is no longer waiting behind it.
                if out.send(units).is_err() {
                    break;
                }
            }
            Err(err) => {
                tracing::warn!("the hardware encoder refused a frame: {err:#}");
                break;
            }
        }
    }

    // The tail. An asynchronous MFT produces output when it decides to, so the last frames of a
    // stream only become real when it is drained.
    if let Some(mut mft) = encoder {
        match mft.finish(MFT_TIMEOUT) {
            Ok(tail) if !tail.is_empty() => {
                if let Err(err) = out.try_send(tail) {
                    tracing::warn!("the hardware encoder's last frames did not fit the queue: {err}");
                }
            }
            Ok(_) => {}
            Err(err) => tracing::warn!("draining the hardware encoder failed: {err:#}"),
        }
    }
}
