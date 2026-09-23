//! WASAPI loopback audio capture of the default render endpoint (spec §5.1).
//!
//! Loopback is the only zero-config way to capture game audio on Windows: there is no
//! DirectShow device for the default output, so ffmpeg's `dshow` input cannot be used
//! without asking the user to install a virtual audio cable.
//!
//! # What has been verified about this code
//!
//! It type-checks for `x86_64-pc-windows-msvc` from macOS (`cargo check` does not
//! link), which is a real gate for API shape, ownership and HRESULT plumbing. It has
//! **never been executed on Windows**: no sample has ever come through it, and neither
//! the sample conversion nor the block pacing has been observed.
//!
//! # Format conversion
//!
//! The endpoint mix format is typically 32-bit float, so samples are converted to the
//! fixed 48kHz stereo s16le the rest of the pipeline assumes (spec §13). Channel
//! handling is deliberately naive: two channels are passed through, mono is duplicated
//! to both sides, and anything wider contributes its first two channels (front left
//! and right) — a mixdown is deferred.
//!
//! There is **no resampling**. A mix format whose sample rate is not 48kHz would
//! otherwise be labelled 48kHz and silently drift against the video timeline, so
//! `start` refuses it by name instead. Changing the endpoint back to 48kHz (the
//! Windows default for shared mode) is the fix until resampling lands.

#![cfg(windows)]

use crate::platform::init_com;
use crate::{clock_base, AudioBackend, AudioBuffer, AudioFormat};
use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};

use windows::core::IUnknown;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, BOOL, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// The format every consumer downstream of this backend assumes: the audio timeline is
/// exactly derivable from the byte count (spec §13).
const TARGET: AudioFormat = AudioFormat { sample_rate: 48_000, channels: 2 };

/// 100-nanosecond units per millisecond, for `IAudioClient::Initialize`.
const HNS_PER_MS: i64 = 10_000;

/// `WAVE_FORMAT_EXTENSIBLE`, i.e. "look at the SubFormat GUID".
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// `WAVE_FORMAT_IEEE_FLOAT`.
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
/// `WAVE_FORMAT_PCM`.
const WAVE_FORMAT_PCM: u16 = 0x0001;

/// Packets drained per `next_buffer` call, so a wedged or pathologically chatty endpoint
/// cannot spin the drain loop forever. Sixteen packets is 160ms of audio, far more than a
/// single call should need to consume; the rest is picked up by the next call.
const MAX_PACKETS_PER_CALL: usize = 16;

/// WASAPI loopback capture of the default render endpoint, converted to 48kHz stereo
/// s16le.
pub struct WasapiLoopback {
    /// Published format; always `TARGET` — see the module docs on resampling.
    format: AudioFormat,
    client: Option<Client>,
    /// Converted samples that do not yet add up to a whole 10ms block.
    pending: Vec<u8>,
    /// pts of the next block to be published, stamped when its first sample arrived.
    pending_pts: Duration,
    last_pts: Duration,
    /// Whether this thread's COM apartment was taken by us and must be given back.
    com_owned: bool,
}

struct Client {
    audio: IAudioClient,
    capture: IAudioCaptureClient,
    event: EventHandle,
    mix: MixFormat,
}

// SAFETY: the backend is used from one thread at a time — the thread that called
// `start`, which is also the only thread allowed to call `stop`, because COM apartments
// are thread-affine and `CoUninitialize` has to run where `CoInitializeEx` did. `Send` is
// required by the `AudioBackend` trait and is sound here only because nothing shares
// these objects concurrently: `IMMDevice`, `IAudioClient` and `IAudioCaptureClient` are
// free-threaded WASAPI objects, and the event is a kernel handle. Moving the backend to
// another thread is safe; *sharing* it is not, and nothing does.
unsafe impl Send for Client {}

/// Owns the auto-reset event the audio engine signals for each new packet.
struct EventHandle(HANDLE);

impl Drop for EventHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateEventW and is closed exactly once, here.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // SAFETY: the client is alive for as long as `self` is; a failure here has
        // nowhere left to be reported.
        unsafe {
            let _ = self.audio.Stop();
        }
    }
}

/// The parts of the endpoint mix format the conversion needs.
struct MixFormat {
    sample_rate: u32,
    channels: u16,
    /// Bytes per frame, channels included.
    block_align: usize,
    sample: SampleKind,
}

/// The sample encoding of the endpoint mix format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleKind {
    /// 32-bit float in [-1, 1].
    F32,
    /// 16-bit signed PCM.
    I16,
    /// 32-bit signed PCM; the top 16 bits are the s16 the pipeline wants.
    I32,
}

impl SampleKind {
    fn bytes(self) -> usize {
        match self {
            Self::F32 | Self::I32 => 4,
            Self::I16 => 2,
        }
    }
}

impl WasapiLoopback {
    pub fn new() -> Result<Self> {
        Ok(Self {
            format: TARGET,
            client: None,
            pending: Vec::new(),
            pending_pts: Duration::ZERO,
            last_pts: Duration::ZERO,
            com_owned: false,
        })
    }

    fn release_com(&mut self) {
        if self.com_owned {
            // SAFETY: `start` took the apartment on this thread.
            unsafe { windows::Win32::System::Com::CoUninitialize() };
            self.com_owned = false;
        }
    }
}

impl AudioBackend for WasapiLoopback {
    fn start(&mut self) -> Result<()> {
        if self.client.is_some() {
            bail!("WASAPI loopback capture is already started");
        }
        self.com_owned = init_com()?;
        match open_client() {
            Ok(client) => {
                self.pending.clear();
                self.pending_pts = Duration::ZERO;
                self.last_pts = Duration::ZERO;
                self.client = Some(client);
                Ok(())
            }
            Err(e) => {
                // Nothing was captured, so hand the apartment back rather than leaving
                // the thread in an apartment nobody owns.
                self.release_com();
                Err(e)
            }
        }
    }

    /// Returns the next whole 10ms block, or `Ok(None)` when nothing is due within
    /// `timeout`. A zero timeout makes this a non-blocking "is anything due?" check,
    /// which is how the CLI drains audio without starving video.
    fn next_buffer(&mut self, timeout: Duration) -> Result<Option<AudioBuffer>> {
        let Self { client, format, pending, pending_pts, last_pts, .. } = self;
        let client = client.as_ref().context("WASAPI loopback capture is not started")?;
        let block_bytes = format.bytes_per_10ms();
        let block_frames = format.sample_rate as usize / 100;
        let block_duration =
            Duration::from_micros(block_frames as u64 * 1_000_000 / format.sample_rate as u64);
        let deadline = Instant::now() + timeout;

        loop {
            // Take everything the engine has already handed us.
            for _ in 0..MAX_PACKETS_PER_CALL {
                let starting_empty = pending.is_empty();
                if !pull_packet(client, pending)? {
                    break;
                }
                if starting_empty {
                    // The block's first sample is the freshest thing we have: stamp it on
                    // the shared clock now, and let further blocks in the same burst
                    // follow on by exactly one block of samples each. This is the moment
                    // the packet was *retrieved*, not the moment it was played — loopback
                    // hands a buffer over about one device period late, so audio
                    // timestamps carry up to ~10ms of offset against video here.
                    // Sample-accurate stamps from the QPC position are deferred.
                    *pending_pts = clock_base().elapsed().max(*last_pts);
                }
            }

            if pending.len() >= block_bytes {
                let data: Vec<u8> = pending.drain(..block_bytes).collect();
                let pts = (*pending_pts).max(*last_pts);
                *last_pts = pts;
                *pending_pts = pts + block_duration;
                return Ok(Some(AudioBuffer { data, frames: block_frames, pts, format: *format }));
            }

            let now = Instant::now();
            if now >= deadline {
                // A partial block stays buffered; it is published once it is complete.
                return Ok(None);
            }
            // The engine signals this event once per device period, so waiting on it is
            // cheaper and more accurate than sleeping for a fixed slice.
            let wait_ms = (deadline - now).as_millis().clamp(1, 200) as u32;
            // SAFETY: `client.event` is a live auto-reset event owned by `self`.
            let signalled = unsafe { WaitForSingleObject(client.event.0, wait_ms) };
            if signalled != WAIT_OBJECT_0 && signalled != WAIT_TIMEOUT {
                bail!("waiting on the WASAPI capture event failed (WAIT_EVENT {})", signalled.0);
            }
        }
    }

    fn stop(&mut self) -> Result<()> {
        // Dropping the client stops the stream and closes the event handle.
        self.client = None;
        self.pending.clear();
        self.release_com();
        Ok(())
    }
}

impl Drop for WasapiLoopback {
    fn drop(&mut self) {
        // A backend dropped without `stop` must still release the endpoint and the
        // COM apartment it took.
        self.client = None;
        self.release_com();
    }
}

/// Open the default render endpoint in loopback mode and start it.
fn open_client() -> Result<Client> {
    // SAFETY: the CLSID is the documented one; `None` means no aggregation.
    let enumerator: IMMDeviceEnumerator = unsafe {
        CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL)
            .context("CoCreateInstance(MMDeviceEnumerator)")?
    };
    // SAFETY: the enumerator is alive; eRender/eConsole is the default output.
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
        .context("IMMDeviceEnumerator::GetDefaultAudioEndpoint(eRender, eConsole)")?;
    // SAFETY: IAudioClient is the render endpoint's documented client interface.
    let audio: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
        .context("IMMDevice::Activate(IAudioClient)")?;

    // SAFETY: the mix format is allocated with CoTaskMemAlloc by the audio engine
    // and freed by `MixFormatGuard` when this function returns.
    let raw_mix = unsafe { audio.GetMixFormat() }.context("IAudioClient::GetMixFormat")?;
    let raw_mix = MixFormatGuard(raw_mix);
    let mix = read_mix_format(raw_mix.0)?;

    if mix.sample_rate != TARGET.sample_rate {
        // Labelling 44.1kHz audio as 48kHz would desync the clip, and there is no
        // resampler yet, so this is a hard error that names the mismatch.
        bail!(
            "the default playback device's mix format is {}Hz, but localplay captures \
             at {}Hz and does not resample yet (deferred to Phase 2). Set the device \
             to 48000Hz in Windows sound settings, or expect audio to be refused here.",
            mix.sample_rate,
            TARGET.sample_rate
        );
    }

    // The event is auto-reset: each wait consumes one signal from the engine.
    // SAFETY: no security attributes and no name are needed for a private event.
    let event = unsafe {
        CreateEventW(None, BOOL::from(false), BOOL::from(false), None)
            .context("CreateEventW for the WASAPI capture event")?
    };
    let event = EventHandle(event);

    // A shared-mode, event-driven, loopback stream. The buffer has to be at least
    // one device period long for the engine to signal the event at all, so ask for
    // the endpoint's own default period.
    let mut default_period = 0i64;
    let mut minimum_period = 0i64;
    // SAFETY: both out-parameters are valid.
    unsafe { audio.GetDevicePeriod(Some(&mut default_period), Some(&mut minimum_period)) }
        .context("IAudioClient::GetDevicePeriod")?;
    let buffer_duration = if default_period > 0 {
        default_period
    } else {
        10 * HNS_PER_MS
    };

    // SAFETY: the stream flags are the documented loopback + event-callback pair;
    // the periodicity argument must be 0 in shared mode, and the format pointer
    // stays valid for the duration of the call.
    unsafe {
        audio
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                buffer_duration,
                0,
                raw_mix.0,
                None,
            )
            .context("IAudioClient::Initialize(loopback)")?;
        audio
            .SetEventHandle(event.0)
            .context("IAudioClient::SetEventHandle")?;
    }
    // SAFETY: registered after Initialize, which is the documented order.
    let capture: IAudioCaptureClient =
        unsafe { audio.GetService() }.context("IAudioClient::GetService(IAudioCaptureClient)")?;
    // SAFETY: the stream is fully configured at this point.
    unsafe { audio.Start() }.context("IAudioClient::Start")?;

    tracing::info!(
        sample_rate = mix.sample_rate,
        channels = mix.channels,
        sample = ?mix.sample,
        "WASAPI loopback capture started on the default render endpoint"
    );
    Ok(Client { audio, capture, event, mix })
}

/// Frees the mix-format blob the audio engine hands back.
struct MixFormatGuard(*mut WAVEFORMATEX);

impl Drop for MixFormatGuard {
    fn drop(&mut self) {
        // SAFETY: `GetMixFormat` allocates with CoTaskMemAlloc and this guard owns the
        // pointer for exactly one free.
        unsafe { CoTaskMemFree(Some(self.0 as *const core::ffi::c_void)) };
    }
}

/// Read the fields the conversion needs out of the endpoint mix format.
fn read_mix_format(format: *const WAVEFORMATEX) -> Result<MixFormat> {
    // SAFETY: `format` points at the live WAVEFORMATEX returned by GetMixFormat, which
    // the caller frees only after this function has copied what it needs out of it.
    // `WAVEFORMATEX` is packed, so every field is copied out by value here rather than
    // borrowed: a reference to an unaligned field would be undefined behaviour.
    let (format_tag, channels, sample_rate, block_align, bits_per_sample, cb_size) = unsafe {
        let base = &*format;
        (
            base.wFormatTag,
            base.nChannels,
            base.nSamplesPerSec,
            base.nBlockAlign as usize,
            base.wBitsPerSample,
            base.cbSize,
        )
    };

    let sample = if format_tag == WAVE_FORMAT_EXTENSIBLE {
        // The SubFormat GUID only exists when cbSize covers WAVEFORMATEXTENSIBLE's
        // extra fields; a shorter blob means the tag is lying.
        if cb_size < 22 {
            bail!(
                "the endpoint mix format claims WAVE_FORMAT_EXTENSIBLE but its cbSize is \
                 {cb_size} (need 22 for the SubFormat GUID)"
            );
        }
        // SAFETY: cbSize >= 22 was just checked, so the extended struct's SubFormat
        // GUID is in bounds. The GUID is copied out for the same alignment reason.
        let sub_format = unsafe { (*(format as *const WAVEFORMATEXTENSIBLE)).SubFormat };
        if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            SampleKind::F32
        } else if sub_format == KSDATAFORMAT_SUBTYPE_PCM {
            pcm_kind(bits_per_sample)?
        } else {
            bail!(
                "unsupported endpoint mix format: WAVE_FORMAT_EXTENSIBLE with SubFormat \
                 {sub_format:?}"
            );
        }
    } else if format_tag == WAVE_FORMAT_IEEE_FLOAT {
        SampleKind::F32
    } else if format_tag == WAVE_FORMAT_PCM {
        pcm_kind(bits_per_sample)?
    } else {
        bail!(
            "unsupported endpoint mix format: wFormatTag {format_tag} (not IEEE float, \
             PCM or extensible)"
        );
    };

    if channels == 0 {
        bail!("the endpoint mix format reports zero channels");
    }
    if sample_rate == 0 {
        bail!("the endpoint mix format reports a zero sample rate");
    }
    let expected = channels as usize * sample.bytes();
    if block_align < expected {
        bail!(
            "the endpoint mix format is inconsistent: nBlockAlign is {block_align} but \
             {channels} channels of {sample:?} need {expected} bytes"
        );
    }

    Ok(MixFormat { sample_rate, channels, block_align, sample })
}

/// The PCM sample width, or an error naming what came instead.
fn pcm_kind(bits_per_sample: u16) -> Result<SampleKind> {
    match bits_per_sample {
        16 => Ok(SampleKind::I16),
        32 => Ok(SampleKind::I32),
        other => bail!("unsupported PCM endpoint format: {other} bits per sample (16 or 32)"),
    }
}

/// Pull one packet from the capture client and append it to `out`, converted.
///
/// Returns whether a packet was consumed. Callers must not assume `out` grew: a packet
/// can be empty, or be flagged silent (in which case its buffer contents are undefined
/// and only the frame count is meaningful).
fn pull_packet(client: &Client, out: &mut Vec<u8>) -> Result<bool> {
    // SAFETY: the capture client is alive for as long as `client` is.
    let available = unsafe { client.capture.GetNextPacketSize() }
        .context("IAudioCaptureClient::GetNextPacketSize")?;
    if available == 0 {
        return Ok(false);
    }

    let mut data: *mut u8 = std::ptr::null_mut();
    let mut frames: u32 = 0;
    let mut flags: u32 = 0;
    // SAFETY: all three are valid out-parameters; the device and QPC positions are not
    // needed because pts comes from the shared Instant base.
    unsafe {
        client
            .capture
            .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
            .context("IAudioCaptureClient::GetBuffer")?
    };

    // The buffer must be released however the conversion below turns out.
    let converted = convert_packet(&client.mix, data, frames, flags, out);
    // SAFETY: `frames` is exactly the count `GetBuffer` just reported.
    let released = unsafe { client.capture.ReleaseBuffer(frames) };
    converted?;
    released.context("IAudioCaptureClient::ReleaseBuffer")?;
    Ok(true)
}

/// Convert one packet's samples into interleaved s16le stereo, appended to `out`.
fn convert_packet(
    mix: &MixFormat,
    data: *mut u8,
    frames: u32,
    flags: u32,
    out: &mut Vec<u8>,
) -> Result<()> {
    let frames = frames as usize;
    if frames == 0 {
        return Ok(());
    }
    let bytes_per_channel = mix.sample.bytes();
    // Two output channels, two bytes per sample.
    out.reserve(frames * 4);

    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
        // A silent packet's buffer is undefined; write the silence it stands for.
        out.resize(out.len() + frames * 4, 0);
        return Ok(());
    }
    if data.is_null() {
        bail!("WASAPI returned a {frames}-frame packet with a null data pointer");
    }
    let byte_len = frames
        .checked_mul(mix.block_align)
        .context("the WASAPI packet size overflowed")?;
    // SAFETY: GetBuffer reports `frames` frames of `block_align` bytes each starting at
    // `data`, and the buffer stays valid until ReleaseBuffer, which happens after this
    // function returns.
    let packet = unsafe { std::slice::from_raw_parts(data, byte_len) };

    // Front left/right only: a mono endpoint doubles its single channel, and anything
    // wider than stereo contributes its first two channels.
    let second_channel = usize::from(mix.channels > 1);
    for frame in 0..frames {
        let base = frame * mix.block_align;
        let left = &packet[base..base + bytes_per_channel];
        let right = &packet
            [base + second_channel * bytes_per_channel..base + (second_channel + 1) * bytes_per_channel];
        out.extend_from_slice(&sample_to_i16(mix.sample, left).to_le_bytes());
        out.extend_from_slice(&sample_to_i16(mix.sample, right).to_le_bytes());
    }
    Ok(())
}

/// One raw sample of the endpoint mix format as s16le.
fn sample_to_i16(kind: SampleKind, raw: &[u8]) -> i16 {
    match kind {
        SampleKind::I16 => i16::from_le_bytes([raw[0], raw[1]]),
        SampleKind::F32 => {
            let float = f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
            // Clamp before scaling: game audio routinely exceeds [-1, 1], and an
            // unclamped cast would wrap a loud sample into a click.
            let clamped = float.clamp(-1.0, 1.0);
            (clamped * f32::from(i16::MAX)).round() as i16
        }
        SampleKind::I32 => {
            // 32-bit PCM kept in the top bits: the s16 value is the high half.
            let wide = i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
            (wide >> 16) as i16
        }
    }
}
