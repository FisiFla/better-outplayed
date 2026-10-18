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
//!
//! # Device changes
//!
//! An invalidated endpoint (headset unplugged) or a new default output reopens the stream on the
//! current default endpoint without resetting the timeline ([`RecoveringSource`]), so the outage
//! is silence in the clip rather than an error or a track that ends early.
//!
//! # Per-application capture
//!
//! With [`GameAudioSource::App`] the running game is captured through process loopback
//! (`ActivateAudioInterfaceAsync` on `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK`, including its child
//! processes), so voice chat and music stay out of the clip. Only the process id is handed to the
//! audio service: the game is not opened, hooked or read. That virtual device follows default
//! output changes by itself. Without a running game, or before Windows build 20348, the whole
//! default output is captured as above ([`GameAudioOpener`]).

#![cfg(windows)]

use crate::audio_recovery::{RecoveringSource, SourceGone};
use crate::audio_target::{GameAudioOpener, LoopbackFactory};
use crate::audio_timeline::{qpc_to_shared, AudioTimeline, PacketInfo, PacketSource};
use crate::platform::init_com;
use crate::{
    clock_base, is_silent_block, AudioBackend, AudioBuffer, AudioFormat, ConvertedSilenceWatch,
    GameAudioSource, GameFinder, NativeAudioFormat, SampleEncoding,
};
use anyhow::{bail, Context, Result};
use std::mem::ManuallyDrop;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use windows::core::{implement, IUnknown, Interface, Ref, HRESULT};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    eConsole, eRender, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_E_DEVICE_INVALIDATED,
    AUDCLNT_E_RESOURCES_INVALIDATED, AUDCLNT_E_SERVICE_NOT_RUNNING, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, IAgileObject, IAgileObject_Impl, BLOB, CLSCTX_ALL,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

/// The format every consumer downstream assumes and the audio engine is asked to deliver, so the
/// audio timeline is exactly derivable from the byte count. Kept in step with [`AudioFormat::default`].
///
/// `pub(crate)` because the microphone backend targets this same format: two tracks in one
/// container have one format between them, and a test pins both backends to it.
pub(crate) const TARGET: AudioFormat = AudioFormat {
    sample_rate: 48_000,
    channels: 2,
};

/// Bytes per captured frame: two channels of 16-bit PCM. Every packet is already in this format.
const BYTES_PER_FRAME: usize = TARGET.channels as usize * 2;

/// 100-nanosecond units per millisecond, for `IAudioClient::Initialize`.
const HNS_PER_MS: i64 = 10_000;

/// The first Windows build Microsoft documents process loopback for.
const PROCESS_LOOPBACK_MIN_BUILD: u32 = 20348;

/// The process-loopback stream buffer. The virtual device has no period to ask for; this is the
/// one Microsoft's ApplicationLoopback sample uses.
const PROCESS_BUFFER_HNS: i64 = 20 * HNS_PER_MS;

/// How long process-loopback activation may take before it is treated as unavailable.
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);

/// `WAVE_FORMAT_EXTENSIBLE`, i.e. "look at the SubFormat GUID".
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_PCM: u16 = 0x0001;

/// WASAPI loopback capture of the game or the default render endpoint, opened as 48kHz stereo
/// s16le with the engine converting (see the module docs).
pub struct WasapiLoopback {
    mode: GameAudioSource,
    /// The output endpoint id whole-output capture listens to; empty is the Windows default.
    output_device: String,
    /// Taken by `start`; a restarted backend captures the whole output.
    find_game: Option<GameFinder>,
    /// The open stream and the timeline it feeds, reopened across device changes.
    source: Option<RecoveringSource<GameAudioOpener<WasapiFactory>>>,
    /// The stream generation `silence_watch` was armed for.
    watched_generation: u64,
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
    /// Whole-output capture.
    pub fn new() -> Result<Self> {
        Self::with_source(GameAudioSource::System, None, "")
    }

    /// `find_game` names the game process to capture under [`GameAudioSource::App`].
    pub fn with_source(
        mode: GameAudioSource,
        find_game: Option<GameFinder>,
        output_device: &str,
    ) -> Result<Self> {
        Ok(Self {
            mode,
            output_device: output_device.to_owned(),
            find_game,
            source: None,
            watched_generation: 0,
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
        if self.source.is_some() {
            bail!("WASAPI loopback capture is already started");
        }
        self.com_owned = init_com()?;
        // Frame 0 is the moment capture began, so a session that opens on silence still starts its
        // audio alongside the video.
        let now = clock_base().elapsed();
        let find_game = match (self.mode, self.find_game.take()) {
            (GameAudioSource::App, Some(find)) if process_loopback_supported() => Some(find),
            (GameAudioSource::App, _) => {
                tracing::warn!(
                    min_build = PROCESS_LOOPBACK_MIN_BUILD,
                    "game_audio = \"app\" needs Windows process loopback, which this system \
                     lacks (or no game list was given); capturing the whole default output"
                );
                None
            }
            (GameAudioSource::System, _) => None,
        };
        let opened = WasapiFactory::new(&self.output_device).and_then(|factory| {
            let opener = GameAudioOpener::new(factory, find_game);
            RecoveringSource::start(opener, AudioTimeline::new(TARGET, now), now)
        });
        match opened {
            Ok(source) => {
                self.source = Some(source);
                self.watched_generation = 0;
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
        let Self {
            source,
            silence_watch,
            watched_generation,
            ..
        } = self;
        let source = source
            .as_mut()
            .context("WASAPI loopback capture is not started")?;
        let deadline = Instant::now() + timeout;

        loop {
            // Take everything the engine has already handed us, then pad any stretch it sent nothing for.
            source.poll(clock_base().elapsed())?;
            if let Some(client) = source.stream() {
                if source.generation() != *watched_generation {
                    // Each endpoint gets its own watch, armed only if it needs the engine's converter.
                    *watched_generation = source.generation();
                    *silence_watch = ConvertedSilenceWatch::new(client.converting);
                }
            }

            if let Some(block) = source.next_block() {
                let buffer = block.buffer;
                let block_duration = Duration::from_micros(
                    buffer.frames as u64 * 1_000_000 / u64::from(TARGET.sample_rate),
                );
                // Diagnostic only: if the endpoint was engine-converted and every block so far was pure
                // silence, this is the reported AUTOCONVERTPCM failure and the user should hear about it
                // rather than find a silent clip. It latches (one warning per session) and never errors or
                // stops capture. Padding is skipped: the engine sending nothing says nothing about conversion.
                if !block.filler
                    && silence_watch.observe(block_duration, is_silent_block(&buffer.data))
                {
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
            let Some(client) = source.stream() else {
                // No stream to wait on while the device is being reopened; silence is padded meanwhile.
                std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
                continue;
            };
            // The engine signals this event once per device period, so waiting on it beats sleeping a fixed slice.
            let wait_ms = (deadline - now).as_millis().clamp(1, 200) as u32;
            // SAFETY: `client.event` is a live auto-reset event owned by `self`.
            let signalled = unsafe { WaitForSingleObject(client.event.0, wait_ms) };
            if signalled != WAIT_OBJECT_0 && signalled != WAIT_TIMEOUT {
                bail!(
                    "waiting on the WASAPI capture event failed (WAIT_EVENT {})",
                    signalled.0
                );
            }
        }
    }

    fn stop(&mut self) -> Result<()> {
        // Dropping the client stops the stream and closes the event handle.
        self.source = None;
        self.release_com();
        Ok(())
    }
}

impl Drop for WasapiLoopback {
    fn drop(&mut self) {
        // A backend dropped without `stop` must still release the endpoint and the COM apartment it took.
        self.source = None;
        self.release_com();
    }
}

/// Opens process loopback on a game, or endpoint loopback on whatever the default output is now.
struct WasapiFactory {
    enumerator: IMMDeviceEnumerator,
    /// The output endpoint id to loop back; empty follows the Windows default output.
    output_device: String,
}

// SAFETY: as for `Client`: only the capture thread uses it, and the enumerator is free-threaded.
unsafe impl Send for WasapiFactory {}

impl WasapiFactory {
    fn new(output_device: &str) -> Result<Self> {
        // SAFETY: the CLSID is the documented one; `None` means no aggregation.
        let enumerator: IMMDeviceEnumerator = unsafe {
            CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL)
                .context("CoCreateInstance(MMDeviceEnumerator)")?
        };
        Ok(Self {
            enumerator,
            output_device: output_device.to_owned(),
        })
    }

    /// The configured output endpoint, or the default output when none is configured.
    fn default_device(&self) -> Result<IMMDevice> {
        if !self.output_device.is_empty() {
            return crate::audio_devices::windows_impl::device_by_id(
                &self.enumerator,
                &self.output_device,
            );
        }
        // SAFETY: the enumerator is alive; eRender/eConsole is the default output.
        unsafe { self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .context("IMMDeviceEnumerator::GetDefaultAudioEndpoint(eRender, eConsole)")
    }
}

impl LoopbackFactory for WasapiFactory {
    type Stream = Client;

    fn open_process(&mut self, pid: u32) -> Result<Client> {
        open_process_client(pid)
    }

    fn open_system(&mut self) -> Result<(Client, String)> {
        let device = self.default_device()?;
        let id = device_id(&device)?;
        Ok((open_client(&device)?, id))
    }

    fn system_key(&mut self) -> Result<String> {
        device_id(&self.default_device()?)
    }
}

/// Whether this Windows build has process loopback. `RtlGetVersion` because `GetVersionEx`
/// reports whatever the application manifest claims.
fn process_loopback_supported() -> bool {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a valid OSVERSIONINFOW with its size set, as the call requires.
    let status = unsafe { RtlGetVersion(&mut info) };
    let supported = status.is_ok() && info.dwBuildNumber >= PROCESS_LOOPBACK_MIN_BUILD;
    tracing::debug!(
        build = info.dwBuildNumber,
        supported,
        "process loopback availability"
    );
    supported
}

/// Signals the waiting thread when process-loopback activation finishes. Agile, because the audio
/// service may call it from any thread.
#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct ActivationDone(mpsc::Sender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationDone_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        // The receiver may have timed out and gone; nothing is left to tell then.
        let _ = self.0.send(());
        Ok(())
    }
}

impl IAgileObject_Impl for ActivationDone_Impl {}

/// Open process loopback on `pid` and its child processes and start it.
fn open_process_client(pid: u32) -> Result<Client> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // A VT_BLOB pointing at `params`, which outlives the activation below. Not dropped through
    // PropVariantClear: the blob is on this stack, not CoTaskMemAlloc'd.
    let activation = ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&raw mut params).cast(),
                    },
                },
            }),
        },
    });
    let (done, completed) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationDone(done).into();
    // SAFETY: the device path is the documented process-loopback constant, the IID matches the
    // interface asked for, and `activation` and `params` stay alive until the result is read.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*activation),
            &handler,
        )
    }
    .context("ActivateAudioInterfaceAsync(process loopback)")?;
    completed
        .recv_timeout(ACTIVATION_TIMEOUT)
        .context("process loopback activation did not complete")?;
    let mut result = HRESULT(0);
    let mut activated: Option<IUnknown> = None;
    // SAFETY: the operation has completed, so its result is ready; both out-parameters are valid.
    unsafe { operation.GetActivateResult(&mut result, &mut activated) }
        .context("IActivateAudioInterfaceAsyncOperation::GetActivateResult")?;
    result
        .ok()
        .with_context(|| format!("process loopback activation for pid {pid}"))?;
    let audio: IAudioClient = activated
        .context("process loopback activation returned no interface")?
        .cast()
        .context("process loopback interface is not an IAudioClient")?;

    // The virtual device has no mix format or period to read; the engine converts into `TARGET`.
    let client = start_stream(audio, PROCESS_BUFFER_HNS, false, || {
        format!(
            "IAudioClient::Initialize(process loopback for pid {pid}, requested 48kHz stereo s16)"
        )
    })?;
    tracing::info!(pid, "WASAPI process loopback capture started");
    Ok(client)
}

/// The endpoint's id string, which tells one device from another across reopens.
fn device_id(device: &IMMDevice) -> Result<String> {
    // SAFETY: the device is alive; the returned string is CoTaskMemAlloc'd and freed right after
    // it is copied out.
    unsafe {
        let id = device.GetId().context("IMMDevice::GetId")?;
        let text = id.to_string();
        CoTaskMemFree(Some(id.0 as *const core::ffi::c_void));
        text.context("IMMDevice::GetId returned a string that is not UTF-16")
    }
}

/// Whether an HRESULT means the endpoint behind the stream is gone, so a reopen can recover.
fn is_endpoint_gone(e: &windows::core::Error) -> bool {
    [
        AUDCLNT_E_DEVICE_INVALIDATED,
        AUDCLNT_E_RESOURCES_INVALIDATED,
        AUDCLNT_E_SERVICE_NOT_RUNNING,
    ]
    .contains(&e.code())
}

/// Context for a capture call, marking a lost endpoint as [`SourceGone`] so it is reopened.
fn capture_call(what: &'static str) -> impl FnOnce(windows::core::Error) -> anyhow::Error {
    move |e| {
        if is_endpoint_gone(&e) {
            SourceGone(format!("{what}: HRESULT {:#010x}", e.code().0)).into()
        } else {
            anyhow::Error::new(e).context(what)
        }
    }
}

/// Open `device` in loopback mode and start it, asking the engine for the canonical 48kHz stereo
/// s16le format whatever the endpoint's own mix format is.
fn open_client(device: &IMMDevice) -> Result<Client> {
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

    let client = start_stream(audio, buffer_duration, native.needs_conversion(), || {
        format!(
            "IAudioClient::Initialize(loopback, AUTOCONVERTPCM, requested {}Hz stereo \
             s16): the audio engine would not open the default render endpoint for \
             capture (it reports {}Hz/{}ch/{:?} natively). No fallback format is used: \
             capturing the endpoint's own format and labelling it 48kHz would \
             desynchronise the clip",
            TARGET.sample_rate, native.sample_rate, native.channels, native.encoding
        )
    })?;

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
    Ok(client)
}

/// Initialise `audio` as an event-driven loopback stream in [`TARGET`] format and start it.
/// `refused` describes a failed `Initialize`.
fn start_stream(
    audio: IAudioClient,
    buffer_duration: i64,
    converting: bool,
    refused: impl FnOnce() -> String,
) -> Result<Client> {
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
            .with_context(refused)?;
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
    Ok(Client {
        audio,
        capture,
        event,
        converting,
        anchor_hns,
        anchor,
    })
}

/// The QPC in 100ns units and the shared clock, read back to back. `Instant` is QPC-based on
/// Windows, so the two then differ by a constant and one reading maps every packet timestamp.
pub(crate) fn qpc_anchor() -> Result<(u64, Duration)> {
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

    NativeAudioFormat {
        sample_rate,
        channels,
        encoding,
    }
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
            .map_err(capture_call("IAudioCaptureClient::GetNextPacketSize"))?;
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
                .GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    None,
                    Some(&mut qpc_position),
                )
                .map_err(capture_call("IAudioCaptureClient::GetBuffer"))?
        };

        // The buffer must be released however the copy below turns out.
        let copied = append_packet(data, frames, flags, out);
        // SAFETY: `frames` is exactly the count `GetBuffer` just reported.
        let released = unsafe { self.capture.ReleaseBuffer(frames) };
        copied?;
        released.map_err(capture_call("IAudioCaptureClient::ReleaseBuffer"))?;

        let timestamp_ok =
            flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 && qpc_position != 0;
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
