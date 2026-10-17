//! WASAPI loopback audio capture of the default render endpoint.
//!
//! Loopback is the only zero-config way to capture game audio on Windows: there is no DirectShow
//! device for the default output, so ffmpeg's `dshow` input would need a virtual audio cable.
//!
//! It type-checks for `x86_64-pc-windows-msvc` from macOS (`cargo check` does not link) but has
//! never been executed on Windows: no sample has come through it, and neither the engine-side
//! format conversion nor the block pacing has been observed.
//!
//! # Format conversion
//!
//! The stream is opened asking for 48kHz stereo s16le, the canonical timeline the pipeline
//! assumes, whatever the endpoint's own mix format is. `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` (with
//! `AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY`) makes the engine's sample rate converter and
//! channel matrixer turn the endpoint's format into ours, so a 44.1kHz USB DAC or 96kHz wireless
//! headset is captured. Nothing converts samples in Rust: `pull_packet` copies each packet
//! verbatim, apart from turning the engine's silent flag into explicit zeros. The endpoint's
//! native format is read only to log it next to the requested one, which is how to confirm the
//! conversion happened.
//!
//! There is deliberately no fallback if `Initialize` with that format fails. Capturing the
//! endpoint's raw format and labelling it 48kHz would desynchronise the audio timeline, so a
//! refusal is loud and names the requested format.
//!
//! # Timing
//!
//! Each packet is placed by its device QPC timestamp, not by how many bytes came before it (see
//! [`AudioTimeline`]): loopback sends nothing while nothing plays, so a byte count alone would
//! play every later sample early against the video.

#![cfg(windows)]

use crate::audio_timeline::{qpc_to_shared, AudioTimeline, PacketInfo, PacketSource};
use crate::platform::init_com;
use crate::{
    clock_base, is_silent_block, AudioBackend, AudioBuffer, AudioFormat, ConvertedSilenceWatch,
    NativeAudioFormat, SampleEncoding,
};
use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};

use windows::core::IUnknown;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// The format every consumer downstream assumes and the audio engine is asked to deliver, so the
/// audio timeline is exactly derivable from the byte count. Kept in step with [`AudioFormat::default`].
///
/// `pub(crate)` because the microphone backend targets this same format: two tracks in one
/// container have one format between them, and a test pins both backends to it.
pub(crate) const TARGET: AudioFormat = AudioFormat { sample_rate: 48_000, channels: 2 };

/// Bytes per captured frame: two channels of 16-bit PCM. Every packet is already in this format.
const BYTES_PER_FRAME: usize = TARGET.channels as usize * 2;

/// 100-nanosecond units per millisecond, for `IAudioClient::Initialize`.
const HNS_PER_MS: i64 = 10_000;

/// `WAVE_FORMAT_EXTENSIBLE`, i.e. "look at the SubFormat GUID".
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_PCM: u16 = 0x0001;

/// Packets drained per `next_buffer` call, so a wedged or chatty endpoint cannot spin the drain
/// loop forever. Sixteen is 160ms of audio; the next call picks up the rest.
const MAX_PACKETS_PER_CALL: usize = 16;

/// WASAPI loopback capture of the default render endpoint, opened as 48kHz stereo s16le with the
/// engine converting (see the module docs).
pub struct WasapiLoopback {
    client: Option<Client>,
    /// Places packets by device time and slices the result into 10ms blocks.
    timeline: AudioTimeline,
    /// Whether this thread's COM apartment was taken by us and must be given back.
    com_owned: bool,
    /// Diagnostic for the reported `AUTOCONVERTPCM`-delivers-silence failure (see
    /// [`ConvertedSilenceWatch`]). Armed when the endpoint needed conversion; it only logs.
    silence_watch: ConvertedSilenceWatch,
}

struct Client {
    audio: IAudioClient,
    capture: IAudioCaptureClient,
    event: EventHandle,
    /// Whether the endpoint's native format needed the engine's converter (was not already 48kHz
    /// stereo s16), so the capture loop can decide whether silence is worth blaming on conversion.
    converting: bool,
    /// One instant read on both the QPC and the shared clock, to move packet timestamps across.
    anchor_hns: u64,
    anchor: Duration,
}

// SAFETY: the backend is used from one thread at a time: the one that called `start`, which is
// also the only one allowed to call `stop`, because COM apartments are thread-affine and
// `CoUninitialize` has to run where `CoInitializeEx` did. `Send` is required by the
// `AudioBackend` trait and is sound only because nothing shares these objects concurrently:
// `IMMDevice`, `IAudioClient` and `IAudioCaptureClient` are free-threaded WASAPI objects and the
// event is a kernel handle. Moving the backend to another thread is safe; sharing it is not,
// and nothing does.
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
        // SAFETY: the client is alive for as long as `self` is; a failure here has nowhere left to be reported.
        unsafe {
            let _ = self.audio.Stop();
        }
    }
}

impl WasapiLoopback {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: None,
            timeline: AudioTimeline::new(TARGET, Duration::ZERO),
            com_owned: false,
            // Disarmed until `start` learns whether the endpoint needed conversion.
            silence_watch: ConvertedSilenceWatch::new(false),
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
                // Frame 0 is the moment capture began, so a session that opens on silence still
                // starts its audio alongside the video.
                self.timeline.reset(client.anchor);
                // A fresh session gets a fresh watch, armed only if the endpoint needed the engine's converter.
                self.silence_watch = ConvertedSilenceWatch::new(client.converting);
                self.client = Some(client);
                Ok(())
            }
            Err(e) => {
                // Nothing was captured, so hand the apartment back rather than leave the thread in one nobody owns.
                self.release_com();
                Err(e)
            }
        }
    }

    /// Returns the next whole 10ms block, or `Ok(None)` when nothing is due within `timeout`. A zero
    /// timeout is a non-blocking "is anything due?" check, how the CLI drains audio without starving video.
    fn next_buffer(&mut self, timeout: Duration) -> Result<Option<AudioBuffer>> {
        let Self { client, timeline, silence_watch, .. } = self;
        let client = client.as_mut().context("WASAPI loopback capture is not started")?;
        let deadline = Instant::now() + timeout;

        loop {
            // Take everything the engine has already handed us, then pad any stretch it sent nothing for.
            if timeline.drain(client, MAX_PACKETS_PER_CALL)? {
                timeline.fill_to(clock_base().elapsed());
            }

            if let Some(block) = timeline.next_block() {
                let buffer = block.buffer;
                let block_duration =
                    Duration::from_micros(buffer.frames as u64 * 1_000_000 / u64::from(TARGET.sample_rate));
                // Diagnostic only: if the endpoint was engine-converted and every block so far was pure
                // silence, this is the reported AUTOCONVERTPCM failure and the user should hear about it
                // rather than find a silent clip. It latches (one warning per session) and never errors or
                // stops capture. Padding is skipped: the engine sending nothing says nothing about conversion.
                if !block.filler && silence_watch.observe(block_duration, is_silent_block(&buffer.data)) {
                    tracing::warn!(
                        silent_run_secs = silence_watch.silent_run().as_secs_f64(),
                        capture_secs = silence_watch.elapsed().as_secs_f64(),
                        "WASAPI loopback has captured only silence, and the default render \
                         endpoint is not natively 48kHz stereo s16, so the audio engine's \
                         sample-rate conversion (AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM) is in \
                         the path. That configuration has been reported to deliver silence \
                         instead of audio on some Windows 11 builds. This is a suspicion, \
                         not a confirmed diagnosis: the clip is still being recorded and \
                         capture has NOT been stopped, but the clip may have no audio. To \
                         rule the conversion out, set the default playback endpoint to \
                         48000 Hz (Settings > System > Sound > the device > Advanced > \
                         Default Format), or report it so the failure mode can be confirmed."
                    );
                }
                return Ok(Some(buffer));
            }

            let now = Instant::now();
            if now >= deadline {
                // A partial block stays buffered until it is complete.
                return Ok(None);
            }
            // The engine signals this event once per device period, so waiting on it beats sleeping a fixed slice.
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
        self.timeline.reset(Duration::ZERO);
        self.release_com();
        Ok(())
    }
}

impl Drop for WasapiLoopback {
    fn drop(&mut self) {
        // A backend dropped without `stop` must still release the endpoint and the COM apartment it took.
        self.client = None;
        self.release_com();
    }
}

/// Open the default render endpoint in loopback mode and start it, asking the engine for the
/// canonical 48kHz stereo s16le format whatever the endpoint's own mix format is.
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

    // The endpoint's native format is read only to be logged next to the request; the stream is not
    // opened with it. A failure here is still fatal: it is the call that proves the endpoint is usable.
    // SAFETY: the native format is allocated with CoTaskMemAlloc by the audio engine and freed by
    // `MixFormatGuard` when this function returns; the fields are copied out before then.
    let raw_mix = unsafe { audio.GetMixFormat() }.context("IAudioClient::GetMixFormat")?;
    let raw_mix = MixFormatGuard(raw_mix);
    let native = native_format(raw_mix.0);

    // What the engine is asked to deliver: our format, not the endpoint's. AUTOCONVERTPCM below has
    // the engine convert into it, and only this format is published downstream.
    let requested = requested_format();

    // The event is auto-reset: each wait consumes one signal from the engine.
    // SAFETY: no security attributes and no name are needed for a private event.
    let event = unsafe {
        CreateEventW(None, false, false, None)
            .context("CreateEventW for the WASAPI capture event")?
    };
    let event = EventHandle(event);

    // A shared-mode, event-driven, loopback stream. The buffer must be at least one device period
    // long for the engine to signal the event at all, so ask for the endpoint's default period.
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

    // AUTOCONVERTPCM has the engine insert a channel matrixer and sample rate converter between the
    // endpoint's mix format and `requested`, and SRC_DEFAULT_QUALITY picks the converter meant for
    // audio a human will hear. Together with loopback and event-driven buffering they let a 44.1kHz
    // USB DAC or 96kHz headset be converted by the engine.
    //
    // Deliberately no fallback to the endpoint's own format if this fails: those bytes would be
    // labelled 48kHz and the audio timeline would drift against the video, so refusing is the
    // fail-closed outcome. The error names the requested format.
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    // SAFETY: the stream flags are the documented loopback + event-callback pair plus the two
    // auto-conversion flags; the periodicity argument must be 0 in shared mode, and `requested`
    // stays alive on this stack frame for the duration of the call.
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
    let (anchor_hns, anchor) = qpc_anchor()?;

    // The endpoint's native format next to the requested one: `converting=true` plus audible 48kHz
    // stereo audio in the clip shows the engine is converting, without a debugger.
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
    Ok(Client { audio, capture, event, converting: native.needs_conversion(), anchor_hns, anchor })
}

/// The QPC in 100ns units and the shared clock, read back to back. `Instant` is QPC-based on
/// Windows, so the two then differ by a constant and one reading maps every packet timestamp.
fn qpc_anchor() -> Result<(u64, Duration)> {
    let mut frequency = 0i64;
    let mut counter = 0i64;
    // SAFETY: both are valid out-parameters for the duration of the calls.
    unsafe {
        QueryPerformanceFrequency(&mut frequency).context("QueryPerformanceFrequency")?;
        QueryPerformanceCounter(&mut counter).context("QueryPerformanceCounter")?;
    }
    let anchor = clock_base().elapsed();
    if frequency <= 0 || counter < 0 {
        bail!("QueryPerformanceCounter returned frequency {frequency}, count {counter}");
    }
    let hns = (counter as u128 * 10_000_000 / frequency as u128) as u64;
    Ok((hns, anchor))
}

/// Frees the mix-format blob the audio engine hands back.
///
/// `pub(crate)` so the microphone backend shares the same call and free, which keeps the two
/// backends' log lines comparable.
pub(crate) struct MixFormatGuard(pub(crate) *mut WAVEFORMATEX);

impl Drop for MixFormatGuard {
    fn drop(&mut self) {
        // SAFETY: `GetMixFormat` allocates with CoTaskMemAlloc and this guard owns the pointer for exactly one free.
        unsafe { CoTaskMemFree(Some(self.0 as *const core::ffi::c_void)) };
    }
}

/// Read the endpoint's native (mix) format, for the log line and nothing else.
///
/// Infallible and non-validating: an unclassifiable format is reported as
/// [`SampleEncoding::Other`], not refused, since the stream is always opened with
/// [`requested_format`]. Shared with the microphone backend.
pub(crate) fn native_format(format: *const WAVEFORMATEX) -> NativeAudioFormat {
    // SAFETY: `format` points at the live WAVEFORMATEX returned by GetMixFormat, which the caller
    // frees only after this function has copied what it needs out of it. `WAVEFORMATEX` is packed,
    // so every field is copied out by value: a reference to an unaligned field would be undefined behaviour.
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
        // The SubFormat GUID exists only when cbSize covers WAVEFORMATEXTENSIBLE's extra fields; a
        // shorter blob means the tag is lying, and there is nothing to report beyond "unrecognised".
        if cb_size < 22 {
            SampleEncoding::Other
        } else {
            // SAFETY: cbSize >= 22 was just checked, so the extended struct's SubFormat GUID is in bounds.
            // The GUID is copied out for the same alignment reason.
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

/// The format the audio engine is asked to deliver: 48kHz stereo, 16-bit PCM, interleaved,
/// `nBlockAlign` 4.
///
/// A plain `WAVEFORMATEX` rather than a `WAVEFORMATEXTENSIBLE`: two channels of 16-bit PCM need no
/// SubFormat GUID, and plain PCM is the classic shared-mode request. With AUTOCONVERTPCM the
/// engine converts the endpoint's audio into it. Shared with the microphone backend.
pub(crate) fn requested_format() -> WAVEFORMATEX {
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

impl PacketSource for Client {
    /// Pull one packet from the capture client and append it to `out`. A packet can be empty, or
    /// flagged silent (its buffer contents are then undefined; only the frame count matters).
    fn pull(&mut self, out: &mut Vec<u8>) -> Result<Option<PacketInfo>> {
        // SAFETY: the capture client is alive for as long as `self` is.
        let available = unsafe { self.capture.GetNextPacketSize() }
            .context("IAudioCaptureClient::GetNextPacketSize")?;
        if available == 0 {
            return Ok(None);
        }

        let mut data: *mut u8 = std::ptr::null_mut();
        let mut frames: u32 = 0;
        let mut flags: u32 = 0;
        let mut qpc_position: u64 = 0;
        // SAFETY: all four are valid out-parameters; the device position is not needed because the
        // QPC position already says when the first frame played.
        unsafe {
            self.capture
                .GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc_position))
                .context("IAudioCaptureClient::GetBuffer")?
        };

        // The buffer must be released however the copy below turns out.
        let copied = append_packet(data, frames, flags, out);
        // SAFETY: `frames` is exactly the count `GetBuffer` just reported.
        let released = unsafe { self.capture.ReleaseBuffer(frames) };
        copied?;
        released.context("IAudioCaptureClient::ReleaseBuffer")?;

        let timestamp_ok = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 && qpc_position != 0;
        Ok(Some(PacketInfo {
            frames: frames as usize,
            time: timestamp_ok
                .then(|| qpc_to_shared(qpc_position, self.anchor_hns, self.anchor))
                .flatten(),
            discontinuity: flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0,
        }))
    }
}

/// Append one packet's samples to `out`.
///
/// The stream was opened as 48kHz stereo s16le, so the engine already produced exactly the bytes
/// the pipeline wants: this is a copy, not a conversion. If the engine handed back another
/// format the byte count would stop matching the 10ms blocks `next_buffer` slices, which is why
/// a failed `Initialize` is a loud error instead of a fallback.
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
    // SAFETY: GetBuffer reports `frames` frames of `BYTES_PER_FRAME` bytes each starting at `data`,
    // and the buffer stays valid until ReleaseBuffer, which happens after this function returns.
    let packet = unsafe { std::slice::from_raw_parts(data, byte_len) };
    out.extend_from_slice(packet);
    Ok(())
}
