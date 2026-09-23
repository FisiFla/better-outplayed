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
//! the engine-side format conversion nor the block pacing has been observed.
//!
//! # Format conversion
//!
//! The stream is opened asking for **48kHz stereo s16le** — the canonical audio
//! timeline the rest of the pipeline assumes (spec §13) — whatever the endpoint's own
//! mix format happens to be. `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` (with
//! `AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY`) inserts the audio engine's sample rate
//! converter and channel matrixer into the path, so `Initialize` is handed *our* format
//! and the engine converts the endpoint's into it: a 44.1kHz USB DAC or a 96kHz
//! wireless headset is captured, where this backend used to refuse to start unless the
//! endpoint was already 48kHz.
//!
//! Nothing converts samples in Rust any more. Every packet arrives as interleaved
//! s16le stereo, 4 bytes per frame, so `pull_packet` copies it out verbatim (only the
//! engine's silent flag still has to be turned into explicit zeros). The endpoint's
//! native format is read, but only to log it next to what was requested: that line,
//! plus a 48kHz stereo audio stream in the produced clip, is how a human on the box
//! confirms the conversion really happened.
//!
//! There is deliberately **no fallback** if `Initialize` with that format fails.
//! Capturing the endpoint's raw format and labelling it 48kHz would desynchronise the
//! audio timeline (spec §13), so a refusal is loud, names the requested format, and
//! never silently produces something else.

#![cfg(windows)]

use crate::platform::init_com;
use crate::{
    clock_base, AudioBackend, AudioBuffer, AudioFormat, NativeAudioFormat, SampleEncoding,
};
use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};

use windows::core::IUnknown;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, BOOL, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// The format every consumer downstream of this backend assumes, and the one the audio
/// engine is asked to deliver: the audio timeline is exactly derivable from the byte
/// count (spec §13). Kept in step with [`AudioFormat::default`], which is the same
/// statement made for the whole workspace.
const TARGET: AudioFormat = AudioFormat { sample_rate: 48_000, channels: 2 };

/// Bytes per captured frame: two channels of 16-bit PCM. Every packet the engine hands
/// back is already in this format, so the byte count is frames x this.
const BYTES_PER_FRAME: usize = TARGET.channels as usize * 2;

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

/// WASAPI loopback capture of the default render endpoint. Whatever the endpoint's
/// native format is, the stream is opened as 48kHz stereo s16le and the audio engine
/// converts (see the module docs).
pub struct WasapiLoopback {
    /// Published format; always `TARGET`, because that is what the engine was asked to
    /// deliver and what it therefore hands back.
    format: AudioFormat,
    client: Option<Client>,
    /// Samples that do not yet add up to a whole 10ms block.
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

/// Open the default render endpoint in loopback mode and start it, asking the audio
/// engine for the pipeline's canonical 48kHz stereo s16le format whatever the endpoint's
/// own mix format is.
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

    // The endpoint's native format is read only to be logged next to what we request: it
    // is *not* what the stream is opened with any more. A failure here is still fatal —
    // it is the same call that proves the endpoint is usable at all, and the log line it
    // feeds is what the runbook checks on the box.
    // SAFETY: the native format is allocated with CoTaskMemAlloc by the audio engine and
    // freed by `MixFormatGuard` when this function returns; the fields are copied out
    // before then.
    let raw_mix = unsafe { audio.GetMixFormat() }.context("IAudioClient::GetMixFormat")?;
    let raw_mix = MixFormatGuard(raw_mix);
    let native = native_format(raw_mix.0);

    // What the engine is asked to deliver. It is *our* format, not the endpoint's,
    // because that is the point of AUTOCONVERTPCM below: the engine converts the
    // endpoint's format into this one, and only this one is ever published downstream.
    let requested = requested_format();

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

    // The two flags that make any endpoint work: AUTOCONVERTPCM has the audio engine
    // insert a channel matrixer and a sample rate converter between the endpoint's mix
    // format and `requested`, and SRC_DEFAULT_QUALITY picks the converter meant for
    // audio a human will hear rather than the cheapest one. Together with loopback and
    // event-driven buffering they are what turns "the endpoint must already be 48kHz"
    // into "44.1kHz USB DACs and 96kHz wireless headsets are converted by the engine".
    //
    // There is deliberately no fallback to the endpoint's own format if this fails:
    // those bytes would be labelled 48kHz and the audio timeline would drift against the
    // video, so refusing is the fail-closed outcome (spec §13). The error names the
    // format that was asked for, because that is the only format this stream would have
    // produced.
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    // SAFETY: the stream flags are the documented loopback + event-callback pair plus the
    // two auto-conversion flags; the periodicity argument must be 0 in shared mode, and
    // `requested` stays alive on this stack frame for the duration of the call.
    unsafe {
        audio
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                buffer_duration,
                0,
                &requested,
                None,
            )
            .with_context(|| {
                format!(
                    "IAudioClient::Initialize(loopback, AUTOCONVERTPCM, requested {}Hz stereo \
                     s16): the audio engine would not open the default render endpoint for \
                     capture (it reports {}Hz/{}ch/{:?} natively). No fallback format is used: \
                     capturing the endpoint's own format and labelling it 48kHz would \
                     desynchronise the clip",
                    TARGET.sample_rate, native.sample_rate, native.channels, native.encoding
                )
            })?;
        audio
            .SetEventHandle(event.0)
            .context("IAudioClient::SetEventHandle")?;
    }
    // SAFETY: registered after Initialize, which is the documented order.
    let capture: IAudioCaptureClient =
        unsafe { audio.GetService() }.context("IAudioClient::GetService(IAudioCaptureClient)")?;
    // SAFETY: the stream is fully configured at this point.
    unsafe { audio.Start() }.context("IAudioClient::Start")?;

    // The runbook reads this line to confirm the engine is converting: it carries the
    // endpoint's native format right next to the format that was requested, so
    // `converting=true` (plus audible 48kHz stereo audio in the clip) is checkable
    // without a debugger. See "Checking an endpoint that is not natively 48 kHz" in
    // docs/runbooks/phase-1-verification.md.
    tracing::info!(
        native_sample_rate = native.sample_rate,
        native_channels = native.channels,
        native_sample_format = ?native.encoding,
        requested_sample_rate = TARGET.sample_rate,
        requested_channels = TARGET.channels,
        requested_sample_format = "s16",
        converting = native.needs_conversion(),
        "WASAPI loopback capture started on the default render endpoint"
    );
    Ok(Client { audio, capture, event })
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

/// Read the endpoint's native (mix) format — for the log line and nothing else.
///
/// Deliberately infallible and non-validating: no sample path depends on it any more, so
/// a format this code cannot classify is reported as [`SampleEncoding::Other`] rather
/// than refused. The stream is always opened with [`requested_format`], and the engine
/// converts whatever it finds.
fn native_format(format: *const WAVEFORMATEX) -> NativeAudioFormat {
    // SAFETY: `format` points at the live WAVEFORMATEX returned by GetMixFormat, which
    // the caller frees only after this function has copied what it needs out of it.
    // `WAVEFORMATEX` is packed, so every field is copied out by value here rather than
    // borrowed: a reference to an unaligned field would be undefined behaviour.
    let (format_tag, channels, sample_rate, bits_per_sample, cb_size) = unsafe {
        let base = &*format;
        (
            base.wFormatTag,
            base.nChannels,
            base.nSamplesPerSec,
            base.wBitsPerSample,
            base.cbSize,
        )
    };

    let encoding = if format_tag == WAVE_FORMAT_EXTENSIBLE {
        // The SubFormat GUID only exists when cbSize covers WAVEFORMATEXTENSIBLE's extra
        // fields; a shorter blob means the tag is lying, and there is nothing to report
        // beyond "not something we recognise".
        if cb_size < 22 {
            SampleEncoding::Other
        } else {
            // SAFETY: cbSize >= 22 was just checked, so the extended struct's SubFormat
            // GUID is in bounds. The GUID is copied out for the same alignment reason.
            let sub_format = unsafe { (*(format as *const WAVEFORMATEXTENSIBLE)).SubFormat };
            if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                SampleEncoding::F32
            } else if sub_format == KSDATAFORMAT_SUBTYPE_PCM {
                pcm_encoding(bits_per_sample)
            } else {
                SampleEncoding::Other
            }
        }
    } else if format_tag == WAVE_FORMAT_IEEE_FLOAT {
        SampleEncoding::F32
    } else if format_tag == WAVE_FORMAT_PCM {
        pcm_encoding(bits_per_sample)
    } else {
        SampleEncoding::Other
    };

    NativeAudioFormat { sample_rate, channels, encoding }
}

/// A PCM sample width as a [`SampleEncoding`]; anything else is [`SampleEncoding::Other`].
fn pcm_encoding(bits_per_sample: u16) -> SampleEncoding {
    match bits_per_sample {
        16 => SampleEncoding::I16,
        32 => SampleEncoding::I32,
        _ => SampleEncoding::Other,
    }
}

/// The format the audio engine is asked to deliver: 48kHz stereo, 16-bit PCM,
/// interleaved — the canonical timeline format (spec §13), `nBlockAlign` 4.
///
/// A plain `WAVEFORMATEX` rather than a `WAVEFORMATEXTENSIBLE`: two channels of 16-bit
/// PCM are fully described without the SubFormat GUID, and plain PCM is the classic
/// shared-mode request. With AUTOCONVERTPCM above, this is the format the engine converts
/// the endpoint's native audio *into*.
fn requested_format() -> WAVEFORMATEX {
    // 16-bit samples, 2 bytes each.
    const BYTES_PER_SAMPLE: u16 = 2;
    let block_align = TARGET.channels * BYTES_PER_SAMPLE;
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM,
        nChannels: TARGET.channels,
        nSamplesPerSec: TARGET.sample_rate,
        nAvgBytesPerSec: TARGET.sample_rate * u32::from(block_align),
        nBlockAlign: block_align,
        wBitsPerSample: 8 * BYTES_PER_SAMPLE,
        cbSize: 0,
    }
}

/// Pull one packet from the capture client and append it to `out`.
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

    // The buffer must be released however the copy below turns out.
    let copied = append_packet(data, frames, flags, out);
    // SAFETY: `frames` is exactly the count `GetBuffer` just reported.
    let released = unsafe { client.capture.ReleaseBuffer(frames) };
    copied?;
    released.context("IAudioCaptureClient::ReleaseBuffer")?;
    Ok(true)
}

/// Append one packet's samples to `out`.
///
/// The stream was opened asking for 48kHz stereo s16le, so the engine's converter has
/// already produced exactly the bytes the pipeline wants: this is a copy, not a
/// conversion. There is no float32→s16 path and no mix-format inspection here because
/// there is nothing left to convert — if the engine handed back another format, its byte
/// count would no longer match the 10ms blocks `next_buffer` slices, which is why a
/// failed `Initialize` is the loud, fail-closed error instead of a fallback.
fn append_packet(data: *mut u8, frames: u32, flags: u32, out: &mut Vec<u8>) -> Result<()> {
    let frames = frames as usize;
    if frames == 0 {
        return Ok(());
    }
    let byte_len = frames
        .checked_mul(BYTES_PER_FRAME)
        .context("the WASAPI packet size overflowed")?;

    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
        // A silent packet's buffer is undefined; write the silence it stands for.
        out.resize(out.len() + byte_len, 0);
        return Ok(());
    }
    if data.is_null() {
        bail!("WASAPI returned a {frames}-frame packet with a null data pointer");
    }
    // SAFETY: GetBuffer reports `frames` frames of `BYTES_PER_FRAME` bytes each starting
    // at `data`, and the buffer stays valid until ReleaseBuffer, which happens after this
    // function returns.
    let packet = unsafe { std::slice::from_raw_parts(data, byte_len) };
    out.extend_from_slice(packet);
    Ok(())
}
