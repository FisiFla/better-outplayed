//! WASAPI **microphone** capture: the default communications *capture* endpoint (spec §5.1).
//!
//! The sibling of `crate::wasapi`. Same `AudioBackend` shape, same canonical published
//! format ([`MICROPHONE_FORMAT`], the crate's `AudioFormat::default`), same 10ms blocks,
//! same [`crate::ConvertedSilenceWatch`] — deliberately the same conventions, because the
//! recorder muxes this track next to the game-audio one and anything that differs between the
//! two backends shows up as drift between the two tracks.
//!
//! # A microphone is a capture endpoint, not a render one
//!
//! `crate::wasapi::WasapiLoopback` opens the default **render** endpoint and sets
//! `AUDCLNT_STREAMFLAGS_LOOPBACK`, which is the flag that makes the engine hand a *render*
//! stream's audio back for reading. Copying that flag here would be exactly wrong: loopback
//! applies to a render endpoint, and a microphone is already an `eCapture` endpoint. It is
//! opened as an ordinary shared-mode capture stream, and `AUDCLNT_STREAMFLAGS_LOOPBACK`
//! appears nowhere in this file — there is a compile-time assertion below that says so.
//! That difference is the whole reason this is a separate backend rather than a parameter on
//! the loopback one.
//!
//! # Which endpoint
//!
//! `GetDefaultAudioEndpoint(eCapture, eCommunications)` — the default **communications**
//! capture device. The communications role is the one Windows assigns to the device the user
//! actually talks into (it is what games, Discord and every other voice path select), so it
//! is what a player means by "the microphone" even on a machine with a webcam mic, a headset
//! mic and a separate audio interface. The data flow is `eCapture` for the same reason in
//! reverse: `eRender` here would hand us a *speaker*.
//!
//! When no communications device has been designated, Windows resolves the role to the
//! machine's default capture device, so a machine with a single microphone needs no second
//! attempt and there is none here. A machine with no capture endpoint at all fails on the
//! call itself, with the role named in the error, rather than falling back to another
//! endpoint the user did not choose.
//!
//! # Format negotiation, and why there is no fallback
//!
//! The stream is opened asking for the canonical **48kHz stereo s16le** timeline format
//! whatever the microphone's own mix format is, with the same technique
//! `crate::wasapi::WasapiLoopback` uses: `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` (plus
//! `AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY`) has the audio engine insert its channel
//! matrixer and sample rate converter between the device and us, so `Initialize` is handed
//! *our* format and the engine converts the device's into it. USB microphones at 44.1kHz, a
//! 96kHz interface input and a mono desktop mic all come out as 48kHz stereo s16 with no
//! conversion code in Rust.
//!
//! The microphone's native format is read (through the same helper the loopback backend uses,
//! so the two log lines are comparable) and logged next to what was requested, together with
//! whether conversion is in the path. If the engine will not open the device in the requested
//! format, `start` **fails loudly** and names both formats: there is deliberately no fallback
//! to the device's own rate or channel count, because publishing the device's format under a
//! 48kHz stereo label would silently corrupt the audio timeline (spec §13).
//!
//! # One channel or two: two, always
//!
//! A microphone may well be mono. The published format is **still stereo**, and this backend
//! never publishes mono, for one reason: the pipeline derives the audio timeline from the byte
//! count (spec §13), so a block carrying one channel's worth of bytes would be read as half
//! its real duration and the microphone track would slide against the game track and the
//! video. Since the stream is opened requesting 2 channels, a mono device is up-mixed 1→2 by
//! the engine's channel matrixer — the single input channel duplicated into L and R, which is
//! what the matrixer does for an equal-channel-count matrix and is exactly the duplication
//! this code would otherwise have to do by hand. Doing it in Rust was the alternative and is
//! strictly worse: it would mean opening the stream in the device's own mono format, which
//! throws away the engine's sample rate conversion along with the mix, and it would leave the
//! WASAPI packets in a format that no longer matches the crate's canonical one. The log line
//! says `engine_upmixes_mono_to_stereo = true` when the device is mono, so which endpoint
//! needed the matrixer is visible in the run.
//!
//! # Block size
//!
//! Ten milliseconds, like the loopback backend: `next_buffer` publishes the same whole blocks
//! [`AudioFormat::bytes_per_10ms`] describes (480 frames, 1920 bytes), and the WASAPI stream
//! buffer is the endpoint's own default device period — the engine signals the event once per
//! device period, so the buffer must be at least that long.
//!
//! # Silence, and a microphone that goes away
//!
//! A muted, muted-in-hardware or disconnected microphone produces digital silence, and
//! silence is not distinguishable from audio by inspecting samples. Every published block is
//! therefore fed to the crate's own [`crate::ConvertedSilenceWatch`], armed exactly when the
//! endpoint needed the engine's converter, which reports a long unbroken run of silence once
//! (never failing and never stopping the capture). Wiring it identically to the loopback
//! backend is the point: the failure mode it names — an auto-converted WASAPI stream that
//! delivers only silence — is not specific to loopback.
//!
//! An endpoint that is *removed* mid-recording (headset unplugged, microphone disabled in
//! Settings, USB device yanked) is a different matter: a shared-mode capture stream delivers a
//! packet every device period whether or not anyone is talking, so a stream that has gone
//! quiet has died, and this backend stops waiting on it. WASAPI's own
//! `AUDCLNT_E_DEVICE_INVALIDATED`/`AUDCLNT_E_RESOURCES_INVALIDATED` are turned into an
//! actionable error, and a stream that has delivered nothing for `DEVICE_STALL` is checked
//! against `IMMDevice::GetState` and fails with a message that names the state. Waiting is
//! bounded by the caller's timeout either way, so a dead endpoint cannot become a hot loop.
//!
//! # What has been verified about this code
//!
//! The Windows half is type-checked for `x86_64-pc-windows-msvc` from macOS (`cargo check`
//! does not link), which is a real gate for API shape, ownership and HRESULT plumbing. It has
//! **never been executed on Windows**: no sample has ever come through it, and neither
//! endpoint selection, nor event-callback pacing, nor the engine accepting the 48kHz stereo
//! s16 request for a real microphone has been observed.

use crate::{AudioBackend, AudioBuffer, AudioFormat};
use std::time::{Duration, Instant};

/// The format the microphone backend publishes, and the one it asks the audio engine for:
/// the crate's canonical [`AudioFormat::default`], spelled out here so that the microphone
/// backend and the system-audio backend can be compared as values (`AudioFormat::default` is
/// the single statement both of them make, and the recorder declares both tracks with it —
/// `-ar 48000 -ac 2`).
pub const MICROPHONE_FORMAT: AudioFormat = AudioFormat { sample_rate: 48_000, channels: 2 };

/// The peak sample of the [`StubMicrophone`]'s default waveform: about −12 dBFS, loud enough
/// that a block is unmistakably not silence and quiet enough to be a plausible voice level.
const DEFAULT_AMPLITUDE: i16 = 8_000;

/// The platform microphone backend, or a clear error on a platform without one.
///
/// On Windows this is [`WasapiMicrophone`], capturing the default communications capture
/// endpoint. Off Windows there is no microphone backend at all — and this returns an error
/// saying so rather than a synthetic stand-in, because a stub selected at runtime would make
/// the recorder look like it is capturing a voice track while recording nothing (the same
/// rule `platform` states for the other backends). [`StubMicrophone`] is the type tests use
/// instead, and it is called out explicitly rather than selected implicitly.
pub fn microphone_backend() -> anyhow::Result<Box<dyn AudioBackend>> {
    #[cfg(windows)]
    {
        Ok(Box::new(WasapiMicrophone::new()?))
    }
    #[cfg(not(windows))]
    {
        Err(no_microphone_on_this_platform())
    }
}

/// The one message every non-Windows microphone entry point returns.
#[cfg(not(windows))]
fn no_microphone_on_this_platform() -> anyhow::Error {
    anyhow::anyhow!(
        "microphone capture is only implemented on Windows (WASAPI): this build has no \
         microphone backend. Record without a microphone, or run on Windows"
    )
}

/// The microphone backend on a host that has no WASAPI.
///
/// It exists so the microphone type has the same name and the same `new()` shape wherever it
/// is compiled, and so a caller that names it off Windows gets this error rather than a type
/// that does not exist. Construction **always fails**: it never silently succeeds with a
/// stand-in. Use [`StubMicrophone`] when a test wants synthetic blocks.
#[cfg(not(windows))]
#[derive(Debug)]
pub struct WasapiMicrophone;

#[cfg(not(windows))]
impl WasapiMicrophone {
    /// Always an error off Windows: there is no WASAPI to capture a microphone with.
    pub fn new() -> anyhow::Result<Self> {
        Err(no_microphone_on_this_platform())
    }
}

/// A microphone backend that yields synthetic blocks, for wiring tests off Windows.
///
/// The recorder's microphone path has to be testable on the machine the suite actually runs
/// on, where there is no WASAPI and therefore no microphone: this stands in for the device,
/// producing canonical 48kHz stereo s16le blocks on the same real-time schedule a real device
/// does, so the recorder's pacing, silence handling and muxing see the shape of stream
/// [`WasapiMicrophone`] would deliver.
///
/// The default signal is deliberately **not** silence: a stub that emitted zeros would let
/// every "audio flowed through the microphone path" assertion pass vacuously. [`Self::with_signal`]
/// is how a test asks for the other case — `with_signal(0)` is a muted microphone, which is
/// what [`crate::ConvertedSilenceWatch`] exists to report.
#[derive(Debug)]
pub struct StubMicrophone {
    format: AudioFormat,
    /// Block length in milliseconds. The pipeline's convention is 10; whole-second blocks are
    /// used by tests that need to reach the 30s silence threshold quickly.
    block_ms: u64,
    /// Peak sample of the emitted square wave; 0 means digital silence.
    amplitude: i16,
    /// pts of the next block, measured from `start` — the schedule *and* the timestamps, from
    /// one counter, so pacing and `pts` can never disagree.
    next_pts: Duration,
    /// Frames emitted in the current run, so the waveform is continuous across blocks rather
    /// than restarting (in phase) at every block boundary.
    phase: u64,
    /// Blocks handed out in the current run, via either path (`next_buffer` or `drain_for`).
    emitted: u64,
    started_at: Option<Instant>,
}

impl StubMicrophone {
    /// A stub producing `format` in blocks of `block_ms` milliseconds, carrying a
    /// deterministic non-silent waveform.
    ///
    /// `block_ms` is clamped to at least 1: a zero-length block would make no progress and
    /// hand the caller an endless supply of empty ones.
    pub fn new(format: AudioFormat, block_ms: u64) -> Self {
        Self {
            format,
            block_ms: block_ms.max(1),
            amplitude: DEFAULT_AMPLITUDE,
            next_pts: Duration::ZERO,
            phase: 0,
            emitted: 0,
            started_at: None,
        }
    }

    /// Emit a waveform whose peak sample is `amplitude`, instead of the default signal.
    ///
    /// `with_signal(0)` is how a test asks for digital silence — a muted or disconnected
    /// microphone, the case [`crate::ConvertedSilenceWatch`] reports. A negative amplitude
    /// inverts the waveform and is still non-silent.
    pub fn with_signal(mut self, amplitude: i16) -> Self {
        self.amplitude = amplitude;
        self
    }

    /// How many blocks this stub has handed out since the last [`AudioBackend::start`].
    ///
    /// Counted for both paths, so a caller can tell how much of a stream a test consumed
    /// without counting in the test body. Reset by `start`, because a restart is a new run.
    pub fn buffers_emitted(&self) -> u64 {
        self.emitted
    }

    /// Produce the blocks covering `elapsed` of timeline without sleeping.
    ///
    /// The pacing-free twin of [`AudioBackend::next_buffer`], mirroring
    /// `StubAudio::drain_for`: tests that need seconds of blocks (the silence watch, for
    /// instance) cannot wait out real time for them. Nothing about the emitted blocks differs
    /// between the two paths.
    pub fn drain_for(&mut self, elapsed: Duration) -> Vec<AudioBuffer> {
        let blocks = (elapsed.as_millis() / self.block_ms as u128) as u64;
        (0..blocks).map(|_| self.next_block()).collect()
    }

    /// Frames in one block. The sample rate is floored at 1 so a nonsensical zero-rate format
    /// cannot divide by zero; the canonical 48kHz 10ms block is 480 frames.
    fn frames_per_block(&self) -> usize {
        ((self.format.sample_rate.max(1) as u128 * self.block_ms as u128) / 1_000) as usize
    }

    /// How long a block lasts, derived from its frame count and the sample rate — the same
    /// derivation the WASAPI backends use, so `pts` and the byte count agree by construction.
    fn block_duration(&self) -> Duration {
        let frames = self.frames_per_block() as u64;
        Duration::from_micros(frames * 1_000_000 / self.format.sample_rate.max(1) as u64)
    }

    /// One block of the signal: a 1kHz square wave of `amplitude`, phase-continuous, written
    /// to every channel.
    ///
    /// A square wave rather than a ramp or a sine: deterministic, trivially checkable (its
    /// peak is exactly `amplitude`), and non-silent from the very first sample — which matters
    /// because a silence check on a partly-silent waveform would be a flaky test.
    fn next_block(&mut self) -> AudioBuffer {
        let frames = self.frames_per_block();
        let channels = self.format.channels as usize;
        // 1kHz: sample_rate/1000 frames per period, forced even so the two half-periods are
        // equal, and floored at 2 so a very low sample rate still alternates.
        let period = ((self.format.sample_rate.max(1) / 1_000).max(2) & !1) as u64;
        let mut data = Vec::with_capacity(frames * channels * 2);
        for _ in 0..frames {
            let sample = if self.phase % period < period / 2 {
                self.amplitude
            } else {
                self.amplitude.wrapping_neg()
            };
            for _ in 0..channels {
                data.extend_from_slice(&sample.to_le_bytes());
            }
            self.phase = self.phase.wrapping_add(1);
        }
        let buffer = AudioBuffer {
            data,
            frames,
            pts: self.next_pts,
            format: self.format,
        };
        self.next_pts += self.block_duration();
        self.emitted += 1;
        buffer
    }
}

impl AudioBackend for StubMicrophone {
    fn start(&mut self) -> anyhow::Result<()> {
        // A restart is a new run: schedule, waveform phase and counter all start together, so
        // one run's blocks can never be compared against another's count.
        self.next_pts = Duration::ZERO;
        self.phase = 0;
        self.emitted = 0;
        self.started_at = Some(Instant::now());
        Ok(())
    }

    /// Real-time paced like `StubAudio::next_buffer`: `Ok(None)` when the next block is not
    /// due within `timeout`, so a zero timeout is a non-blocking "is anything due?" check and
    /// a single-threaded caller can drain the microphone and the video without starving
    /// either. `drain_for` is the pacing-free path tests use.
    fn next_buffer(&mut self, timeout: Duration) -> anyhow::Result<Option<AudioBuffer>> {
        let started = self
            .started_at
            .ok_or_else(|| anyhow::anyhow!("microphone capture not started"))?;
        let Some(sleep) = self.next_pts.checked_sub(started.elapsed()) else {
            return Ok(Some(self.next_block())); // already due
        };
        if sleep > timeout {
            return Ok(None);
        }
        std::thread::sleep(sleep);
        Ok(Some(self.next_block()))
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// The real microphone backend: WASAPI shared-mode capture from the default communications
/// capture endpoint.
#[cfg(windows)]
mod wasapi_impl {
    use crate::platform::init_com;
    use crate::{
        clock_base, is_silent_block, AudioBackend, AudioBuffer, AudioFormat, ConvertedSilenceWatch,
    };
    // The native-format reader, the mix-format guard and the canonical requested format are
    // shared with the loopback backend rather than copied: they are the same three questions
    // asked of a different endpoint, and one implementation is what keeps the two backends'
    // log lines comparable and their published format identical.
    use crate::wasapi::{native_format, requested_format, MixFormatGuard, TARGET};
    use anyhow::{bail, Context, Result};
    use std::time::{Duration, Instant};
    use windows::core::IUnknown;
    use windows::Win32::Foundation::{BOOL, CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::Media::Audio::{
        eCapture, eCommunications, IAudioCaptureClient, IAudioClient, IMMDevice,
        IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT,
        AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_RESOURCES_INVALIDATED, AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    /// Bytes per captured frame: two channels of 16-bit PCM. Every packet the engine hands back
    /// is already in this format — stereo, whatever the device is (see the module docs on mono
    /// up-mixing) — so the byte count is frames x this, and the timeline is derivable from it.
    const BYTES_PER_FRAME: usize = TARGET.channels as usize * 2;

    /// 100-nanosecond units per millisecond, for `IAudioClient::Initialize`.
    const HNS_PER_MS: i64 = 10_000;

    /// Packets drained per `next_buffer` call, so a pathologically chatty device cannot spin
    /// the drain loop forever. Sixteen packets is 160ms of audio; the rest is picked up by the
    /// next call.
    const MAX_PACKETS_PER_CALL: usize = 16;

    /// How long the stream may deliver nothing before the endpoint's state is checked.
    ///
    /// A live shared-mode capture stream delivers a packet every device period (~10ms) whether
    /// or not anyone is talking: a muted microphone is silent *packets*, not an absence of
    /// packets. So a stretch this long with nothing at all means the endpoint has gone away.
    /// Two seconds is short enough to report a headset yanked out mid-recording while the user
    /// still cares, and long enough that a device that is merely slow to start streaming cannot
    /// trip it.
    const DEVICE_STALL: Duration = Duration::from_secs(2);

    /// The flags this backend initialises the stream with.
    ///
    /// `EVENTCALLBACK` is what makes the engine signal per packet instead of us polling;
    /// `AUTOCONVERTPCM` + `SRC_DEFAULT_QUALITY` insert the engine's channel matrixer and
    /// sample rate converter so the stream can be opened as the crate's canonical format
    /// whatever the device is. `AUDCLNT_STREAMFLAGS_LOOPBACK` is deliberately **absent**: it
    /// applies to a render endpoint, and this stream is an `eCapture` endpoint. Naming the
    /// flags here rather than at the call site is what lets the assertion below guard that.
    const STREAM_FLAGS: u32 = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

    // A microphone is a capture endpoint: asking for loopback here would be a category error
    // (the flag exists to capture a *render* endpoint). This is a compile-time pin, not a
    // comment, because it is the single most likely way to break this backend by copy-paste.
    const _: () = assert!(
        STREAM_FLAGS & AUDCLNT_STREAMFLAGS_LOOPBACK == 0,
        "AUDCLNT_STREAMFLAGS_LOOPBACK is for capturing a render endpoint; the microphone is an \
         eCapture endpoint and must be opened without it"
    );

    /// WASAPI shared-mode microphone capture. Whatever the device's native format is, the
    /// stream is opened as 48kHz stereo s16le and the audio engine converts (see the module
    /// docs).
    pub struct WasapiMicrophone {
        /// Published format; always [`TARGET`], because that is what the engine was asked to
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
        /// The diagnostic for a microphone that is delivering nothing but silence (see
        /// [`ConvertedSilenceWatch`]). Armed when the endpoint needed engine conversion; it
        /// only ever logs, and never fails or stops the capture.
        silence_watch: ConvertedSilenceWatch,
        /// When a packet last came out of the engine. Persisted across `next_buffer` calls
        /// because the caller polls in short slices, so a per-call timestamp could never reach
        /// [`DEVICE_STALL`].
        last_packet_at: Instant,
        /// When the endpoint's state was last checked, so a stalled stream asks Windows at
        /// most once per [`DEVICE_STALL`] rather than once per poll.
        last_state_check: Instant,
    }

    struct Client {
        audio: IAudioClient,
        capture: IAudioCaptureClient,
        /// Kept for one reason: `GetState` is how a stream that has stopped delivering packets
        /// is told apart from a device that is gone.
        device: IMMDevice,
        event: EventHandle,
        /// Whether the endpoint's native format needed the engine's converter (i.e. was not
        /// already 48kHz stereo s16). Recorded here so the capture loop can decide whether a
        /// run of silence is worth blaming on the conversion.
        converting: bool,
    }

    // SAFETY: the backend is used from one thread at a time — the thread that called `start`,
    // which is also the only thread allowed to call `stop`, because COM apartments are
    // thread-affine and `CoUninitialize` has to run where `CoInitializeEx` did. `Send` is
    // required by the `AudioBackend` trait and is sound here only because nothing shares these
    // objects concurrently: `IMMDevice`, `IAudioClient` and `IAudioCaptureClient` are
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
            // SAFETY: the client is alive for as long as `self` is; a failure here has nowhere
            // left to be reported.
            unsafe {
                let _ = self.audio.Stop();
            }
        }
    }

    impl WasapiMicrophone {
        /// A microphone backend that will open the default communications capture endpoint in
        /// [`AudioBackend::start`].
        ///
        /// Construction touches no device: nothing is opened, no COM apartment is taken and
        /// nothing can fail, which is what lets the recorder build its backend selection before
        /// it decides to record. Endpoint and format errors therefore surface from `start`,
        /// where they can be reported with the rest of the recording's startup errors.
        pub fn new() -> Result<Self> {
            Ok(Self {
                format: TARGET,
                client: None,
                pending: Vec::new(),
                pending_pts: Duration::ZERO,
                last_pts: Duration::ZERO,
                com_owned: false,
                // Disarmed until `start` learns whether the endpoint needed conversion.
                silence_watch: ConvertedSilenceWatch::new(false),
                last_packet_at: Instant::now(),
                last_state_check: Instant::now(),
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

    impl AudioBackend for WasapiMicrophone {
        fn start(&mut self) -> Result<()> {
            if self.client.is_some() {
                bail!("WASAPI microphone capture is already started");
            }
            self.com_owned = init_com()?;
            match open_client() {
                Ok(client) => {
                    self.pending.clear();
                    self.pending_pts = Duration::ZERO;
                    self.last_pts = Duration::ZERO;
                    // A fresh session gets a fresh watch, armed only if this endpoint actually
                    // needed the engine's converter. Moved before `client` is stored below.
                    self.silence_watch = ConvertedSilenceWatch::new(client.converting);
                    self.last_packet_at = Instant::now();
                    self.last_state_check = Instant::now();
                    self.client = Some(client);
                    Ok(())
                }
                Err(e) => {
                    // Nothing was captured, so hand the apartment back rather than leaving the
                    // thread in an apartment nobody owns.
                    self.release_com();
                    Err(e)
                }
            }
        }

        /// Returns the next whole 10ms block, or `Ok(None)` when nothing is due within
        /// `timeout`. A zero timeout makes this a non-blocking "is anything due?" check, which
        /// is how the recorder drains the microphone without starving the video.
        fn next_buffer(&mut self, timeout: Duration) -> Result<Option<AudioBuffer>> {
            let Self {
                client,
                format,
                pending,
                pending_pts,
                last_pts,
                silence_watch,
                last_packet_at,
                last_state_check,
                ..
            } = self;
            let client = client
                .as_ref()
                .context("WASAPI microphone capture is not started")?;
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
                    *last_packet_at = Instant::now();
                    if starting_empty {
                        // The block's first sample is the freshest thing we have: stamp it on
                        // the shared clock now, and let further blocks in the same burst follow
                        // on by exactly one block of samples each. This is the moment the
                        // packet was *retrieved*, not the moment it was spoken into the
                        // microphone — the engine hands a capture buffer over about one device
                        // period late, so this track's timestamps can carry up to ~10ms of
                        // offset against the video. Sample-accurate stamps from the QPC position
                        // are deferred, exactly as on the loopback side.
                        *pending_pts = clock_base().elapsed().max(*last_pts);
                    }
                }

                if pending.len() >= block_bytes {
                    let data: Vec<u8> = pending.drain(..block_bytes).collect();
                    let pts = (*pending_pts).max(*last_pts);
                    *last_pts = pts;
                    *pending_pts = pts + block_duration;
                    let buffer = AudioBuffer { data, frames: block_frames, pts, format: *format };

                    // Diagnostic only: a run of silence with the engine's converter in the path
                    // is either a muted microphone or the reported AUTOCONVERTPCM-delivers-
                    // silence failure mode, and a user whose clip has no voice track needs to
                    // hear about it rather than discover it later. The watch latches, so this
                    // warns at most once per session, and it never errors and never stops
                    // capture: the recording continues either way.
                    if silence_watch.observe(block_duration, is_silent_block(&buffer.data)) {
                        tracing::warn!(
                            silent_run_secs = silence_watch.silent_run().as_secs_f64(),
                            capture_secs = silence_watch.elapsed().as_secs_f64(),
                            "the microphone has captured only silence, and the default \
                             communications capture endpoint is not natively 48kHz stereo s16, \
                             so the audio engine's converter (AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM) \
                             is in the path. Either the microphone is muted, muted in software, \
                             or disconnected, or this is the failure mode reported on some \
                             Windows 11 builds where an auto-converted stream delivers silence \
                             instead of audio. This is a suspicion, not a confirmed diagnosis: \
                             capture is still running and has NOT been stopped, but the clip's \
                             microphone track may be silent. Check the microphone's mute switch \
                             and its level in Settings > System > Sound > Input, and if the \
                             device is already 48kHz stereo it is the input rather than the \
                             converter."
                        );
                    }
                    return Ok(Some(buffer));
                }

                // Nothing has arrived for a while. A live capture endpoint cannot be silent at
                // the API level — a muted microphone is packets flagged silent, not an absence
                // of packets — so ask Windows whether the device is still there. This is the
                // unplugged-headset path, and it must become neither an endless wait nor a hot
                // loop.
                //
                // Checked *here*, ahead of the deadline, rather than after the wait below: the
                // recorder drains this backend with a zero timeout (`pump_once_counted`), and a
                // zero-timeout call returns before it ever reaches the wait, so a check placed
                // there would not run in the real capture loop at all. Throttled to one
                // `GetState` per [`DEVICE_STALL`], so checking on every poll costs nothing.
                let since_packet = last_packet_at.elapsed();
                if since_packet >= DEVICE_STALL && last_state_check.elapsed() >= DEVICE_STALL {
                    *last_state_check = Instant::now();
                    check_endpoint_alive(client, since_packet)?;
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
                    bail!(
                        "waiting on the WASAPI microphone capture event failed (WAIT_EVENT {})",
                        signalled.0
                    );
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

    impl Drop for WasapiMicrophone {
        fn drop(&mut self) {
            // A backend dropped without `stop` must still release the endpoint and the COM
            // apartment it took.
            self.client = None;
            self.release_com();
        }
    }

    /// Open the default communications capture endpoint, in shared mode, and start it, asking
    /// the audio engine for the pipeline's canonical 48kHz stereo s16le format whatever the
    /// device's own mix format is.
    fn open_client() -> Result<Client> {
        // SAFETY: the CLSID is the documented one; `None` means no aggregation.
        let enumerator: IMMDeviceEnumerator = unsafe {
            CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL)
                .context("CoCreateInstance(MMDeviceEnumerator)")?
        };
        // SAFETY: the enumerator is alive. eCapture is the data flow that has microphones in it
        // (eRender would be a speaker), and eCommunications is the role Windows gives the device
        // the user talks into — the one a game's voice chat selects. No loopback flag: this is
        // the endpoint itself, not a render stream handed back.
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, eCommunications) }
            .context(
                "IMMDeviceEnumerator::GetDefaultAudioEndpoint(eCapture, eCommunications): this \
                 machine has no default communications microphone. Plug in or enable a \
                 microphone, or pick one as the default input device in Settings > System > \
                 Sound > Input",
            )?;
        // SAFETY: IAudioClient is the capture endpoint's documented client interface.
        let audio: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .context("IMMDevice::Activate(IAudioClient)")?;

        // The device's native format is read only to be logged next to what we request: it is
        // *not* what the stream is opened with. A failure here is still fatal — it is the same
        // call that proves the endpoint is usable at all, and the log line it feeds is what the
        // runbook checks on the box.
        // SAFETY: the native format is allocated with CoTaskMemAlloc by the audio engine and
        // freed by `MixFormatGuard` when this function returns; the fields are copied out before
        // then.
        let raw_mix = unsafe { audio.GetMixFormat() }.map_err(wasapi_call("IAudioClient::GetMixFormat"))?;
        let raw_mix = MixFormatGuard(raw_mix);
        let native = native_format(raw_mix.0);

        // What the engine is asked to deliver: *our* format, not the device's, because that is
        // the point of AUTOCONVERTPCM below.
        let requested = requested_format();

        // The event is auto-reset: each wait consumes one signal from the engine.
        // SAFETY: no security attributes and no name are needed for a private event.
        let event = unsafe {
            CreateEventW(None, BOOL::from(false), BOOL::from(false), None)
                .context("CreateEventW for the WASAPI capture event")?
        };
        let event = EventHandle(event);

        // A shared-mode, event-driven capture stream. The buffer has to be at least one device
        // period long for the engine to signal the event at all, so ask for the endpoint's own
        // default period.
        let mut default_period = 0i64;
        let mut minimum_period = 0i64;
        // SAFETY: both out-parameters are valid.
        unsafe { audio.GetDevicePeriod(Some(&mut default_period), Some(&mut minimum_period)) }
            .map_err(wasapi_call("IAudioClient::GetDevicePeriod"))?;
        let buffer_duration = if default_period > 0 { default_period } else { 10 * HNS_PER_MS };

        // There is deliberately no fallback to the device's own format if this fails: those
        // bytes would be labelled 48kHz stereo and the audio timeline (derived from the byte
        // count, spec §13) would drift against the game track and the video, so refusing is the
        // fail-closed outcome. The error names the format that was asked for and the format the
        // device reports, because those two together are the whole diagnosis.
        // SAFETY: the stream flags are documented shared-mode flags (see `STREAM_FLAGS`); the
        // periodicity argument must be 0 in shared mode, and `requested` stays alive on this
        // stack frame for the duration of the call.
        unsafe {
            audio
                .Initialize(AUDCLNT_SHAREMODE_SHARED, STREAM_FLAGS, buffer_duration, 0, &requested, None)
                .with_context(|| {
                    format!(
                        "IAudioClient::Initialize(microphone, AUTOCONVERTPCM, requested {}Hz stereo \
                         s16): the audio engine would not open the default communications capture \
                         endpoint in that format (the device reports {}Hz/{}ch/{:?} natively). No \
                         fallback format is used: recording the device's own format and labelling \
                         it 48kHz would desynchronise the microphone track from the game track and \
                         the video",
                        TARGET.sample_rate, native.sample_rate, native.channels, native.encoding
                    )
                })?;
            audio
                .SetEventHandle(event.0)
                .map_err(wasapi_call("IAudioClient::SetEventHandle"))?;
        }
        // SAFETY: registered after Initialize, which is the documented order.
        let capture: IAudioCaptureClient = unsafe { audio.GetService() }
            .map_err(wasapi_call("IAudioClient::GetService(IAudioCaptureClient)"))?;
        // SAFETY: the stream is fully configured at this point.
        unsafe { audio.Start() }.map_err(wasapi_call("IAudioClient::Start"))?;

        // The runbook reads this line to see which microphone was opened and whether the engine
        // is converting for it. `converting = true` plus audible 48kHz stereo audio in the
        // clip's microphone track is checkable without a debugger; `engine_upmixes_mono_to_stereo`
        // says the device was mono and the matrixer duplicated it (see the module docs).
        tracing::info!(
            endpoint_data_flow = "eCapture",
            endpoint_role = "eCommunications",
            native_sample_rate = native.sample_rate,
            native_channels = native.channels,
            native_sample_format = ?native.encoding,
            requested_sample_rate = TARGET.sample_rate,
            requested_channels = TARGET.channels,
            requested_sample_format = "s16",
            converting = native.needs_conversion(),
            engine_upmixes_mono_to_stereo = native.channels == 1,
            "WASAPI microphone capture started on the default communications capture endpoint"
        );
        Ok(Client { audio, capture, device, event, converting: native.needs_conversion() })
    }

    /// Fail cleanly if the endpoint behind a stalled stream has gone away.
    ///
    /// Only the state is consulted, and only when the stream has already gone quiet: a device
    /// that is still active is not an error, however quiet the room is (that is
    /// [`ConvertedSilenceWatch`]'s job, and it only warns). A device that is not active can
    /// never deliver another packet, so the stream is dead and saying so is more useful than
    /// waiting forever.
    fn check_endpoint_alive(client: &Client, since_packet: Duration) -> Result<()> {
        // SAFETY: the device is alive for at least as long as `client`, which outlives this call.
        let state = unsafe { client.device.GetState() }
            .map_err(wasapi_call("IMMDevice::GetState"))?;
        if state.0 & DEVICE_STATE_ACTIVE.0 == 0 {
            bail!(
                "the microphone endpoint has delivered no packet for {:.1}s and is no longer \
                 active (DEVICE_STATE {:#x}): it was unplugged, disabled or removed, and a WASAPI \
                 capture stream does not recover from that. Stop the recording and check the \
                 microphone in Settings > System > Sound > Input",
                since_packet.as_secs_f64(),
                state.0
            );
        }
        Ok(())
    }

    /// Whether an HRESULT means "the endpoint behind this stream is gone".
    ///
    /// `AUDCLNT_E_DEVICE_INVALIDATED` is the documented reply for a device that was removed or
    /// disabled while a stream was running; `AUDCLNT_E_RESOURCES_INVALIDATED` is its sibling for
    /// the resources a stream was holding. Both are permanent for the stream in hand.
    fn is_endpoint_gone(e: &windows::core::Error) -> bool {
        e.code() == AUDCLNT_E_DEVICE_INVALIDATED || e.code() == AUDCLNT_E_RESOURCES_INVALIDATED
    }

    /// Context for a WASAPI call, with the unplugged-microphone case spelled out.
    ///
    /// The same HRESULT comes back for "the device was yanked out" and for everything else, and
    /// on its own it reads as a bare `0x88890004` in a log file. A user with a disconnected
    /// headset can act on the sentence; they cannot act on the number.
    fn wasapi_call(what: &'static str) -> impl FnOnce(windows::core::Error) -> anyhow::Error {
        move |e| {
            if is_endpoint_gone(&e) {
                anyhow::anyhow!(
                    "{what}: the microphone endpoint was removed, unplugged or disabled \
                     (HRESULT {:#010x}); this capture stream cannot recover",
                    e.code().0
                )
            } else {
                anyhow::Error::new(e).context(what)
            }
        }
    }

    /// Pull one packet from the capture client and append it to `out`.
    ///
    /// Returns whether a packet was consumed. Callers must not assume `out` grew: a packet can
    /// be empty, or be flagged silent (in which case its buffer contents are undefined and only
    /// the frame count is meaningful).
    fn pull_packet(client: &Client, out: &mut Vec<u8>) -> Result<bool> {
        // SAFETY: the capture client is alive for as long as `client` is.
        let available = unsafe { client.capture.GetNextPacketSize() }
            .map_err(wasapi_call("IAudioCaptureClient::GetNextPacketSize"))?;
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
                .map_err(wasapi_call("IAudioCaptureClient::GetBuffer"))?
        };

        // The buffer must be released however the copy below turns out.
        let copied = append_packet(data, frames, flags, out);
        // SAFETY: `frames` is exactly the count `GetBuffer` just reported.
        let released = unsafe { client.capture.ReleaseBuffer(frames) }
            .map_err(wasapi_call("IAudioCaptureClient::ReleaseBuffer"));
        copied?;
        released?;
        Ok(true)
    }

    /// Append one packet's samples to `out`.
    ///
    /// The stream was opened asking for 48kHz stereo s16le, so the engine's converter has
    /// already produced exactly the bytes the pipeline wants: this is a copy, not a conversion.
    /// There is no float32→s16 path and no mix-format inspection here because there is nothing
    /// left to convert — including the channel count: a mono device was up-mixed 1→2 by the
    /// engine's matrixer when the stream was opened as stereo, and if the engine had not done
    /// that, the byte count of every packet would stop matching the 10ms blocks `next_buffer`
    /// slices (and the timeline derived from it, spec §13) — which is why a failed `Initialize`
    /// is the loud, fail-closed error instead of a fallback.
    fn append_packet(data: *mut u8, frames: u32, flags: u32, out: &mut Vec<u8>) -> Result<()> {
        let frames = frames as usize;
        if frames == 0 {
            return Ok(());
        }
        let byte_len = frames
            .checked_mul(BYTES_PER_FRAME)
            .context("the WASAPI microphone packet size overflowed")?;

        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
            // A silent packet's buffer is undefined; write the silence it stands for. A muted
            // microphone arrives exactly here, which is what lets the silence watch above see it.
            out.resize(out.len() + byte_len, 0);
            return Ok(());
        }
        if data.is_null() {
            bail!("WASAPI returned a {frames}-frame microphone packet with a null data pointer");
        }
        // SAFETY: GetBuffer reports `frames` frames of `BYTES_PER_FRAME` bytes each starting at
        // `data`, and the buffer stays valid until ReleaseBuffer, which happens after this
        // function returns.
        let packet = unsafe { std::slice::from_raw_parts(data, byte_len) };
        out.extend_from_slice(packet);
        Ok(())
    }
}

#[cfg(windows)]
pub use wasapi_impl::WasapiMicrophone;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{is_silent_block, ConvertedSilenceWatch, CONVERTED_SILENCE_THRESHOLD};

    /// One 10ms block of the canonical format, as a frame count.
    const BLOCK_FRAMES: usize = 480;

    fn mic(block_ms: u64) -> StubMicrophone {
        StubMicrophone::new(AudioFormat::default(), block_ms)
    }

    #[test]
    fn the_stub_microphone_emits_the_canonical_format_in_whole_blocks() {
        let mut mic = mic(10);
        mic.start().expect("start the stub microphone");
        let block = mic
            .next_buffer(Duration::from_secs(1))
            .expect("poll the stub")
            .expect("the block at pts 0 is due immediately");

        // The pipeline declares its audio input as `-ar 48000 -ac 2` s16le and derives the
        // audio timeline from the byte count (spec §13), so these three assertions are the
        // whole contract the recorder relies on.
        assert_eq!(block.format, AudioFormat::default(), "48kHz stereo");
        assert_eq!(block.frames, BLOCK_FRAMES, "10ms of 48kHz");
        assert_eq!(
            block.data.len(),
            block.frames * block.format.channels as usize * 2,
            "byte length must be exactly frames x channels x 2"
        );
        assert_eq!(block.data.len(), AudioFormat::default().bytes_per_10ms(), "1920 bytes");
        assert_eq!(block.pts, Duration::ZERO, "the first block is stamped at the run's origin");
    }

    #[test]
    fn the_stub_microphone_is_not_silent_by_default() {
        let mut mic = mic(10);
        mic.start().expect("start the stub microphone");
        let block = mic
            .next_buffer(Duration::from_secs(1))
            .expect("poll the stub")
            .expect("a block is due");

        // A silent default would make every "the microphone path carried audio" assertion in
        // the recorder's tests pass vacuously, and would trip the converter-silence watch in
        // any test that wires it up.
        assert!(
            !is_silent_block(&block.data),
            "the default stub microphone must produce real audio, not zeros"
        );
        let peak = block
            .data
            .chunks_exact(2)
            .map(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs())
            .max()
            .expect("a block has samples");
        assert_eq!(peak, DEFAULT_AMPLITUDE as u16, "and it is the documented default level");
    }

    #[test]
    fn with_signal_scales_the_waveform_and_zero_is_silence() {
        let mut loud = mic(10).with_signal(1_234);
        let mut silent = mic(10).with_signal(0);
        loud.start().expect("start the loud stub");
        silent.start().expect("start the silent stub");
        let loud_block = loud
            .next_buffer(Duration::from_secs(1))
            .expect("poll the loud stub")
            .expect("a block is due");
        let silent_block = silent
            .next_buffer(Duration::from_secs(1))
            .expect("poll the silent stub")
            .expect("a block is due");

        let peak = loud_block
            .data
            .chunks_exact(2)
            .map(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs())
            .max()
            .expect("a block has samples");
        assert_eq!(peak, 1_234, "the waveform's peak is the amplitude that was asked for");
        assert_eq!(
            silent_block.data.len(),
            silent_block.frames * silent_block.format.channels as usize * 2,
            "a silent block keeps the format invariant"
        );
        assert!(
            is_silent_block(&silent_block.data),
            "with_signal(0) is how a test asks for a muted microphone"
        );
    }

    #[test]
    fn the_stub_microphone_pts_advances_monotonically_by_the_block_duration() {
        let mut mic = mic(10);
        mic.start().expect("start the stub microphone");
        let blocks: Vec<AudioBuffer> = (0..5)
            .map(|_| {
                mic.next_buffer(Duration::from_secs(1))
                    .expect("poll the stub")
                    .expect("a block is due")
            })
            .collect();

        // pts advances by exactly one block of samples, never by wall-clock time: that is what
        // makes the microphone track's timeline derivable from its byte count.
        for pair in blocks.windows(2) {
            assert_eq!(
                pair[1].pts,
                pair[0].pts + Duration::from_millis(10),
                "one 10ms block of pts per 10ms block of samples"
            );
        }
        assert!(blocks.windows(2).all(|w| w[0].pts < w[1].pts), "and never goes backwards");
        // 5 blocks handed out, counted by the backend rather than by the test.
        assert_eq!(mic.buffers_emitted(), 5);
    }

    #[test]
    fn a_silent_stub_microphone_is_reported_by_the_converted_silence_watch() {
        // One-second blocks, so the crate's 30s threshold is reached in 30 iterations instead
        // of 3000. The threshold is the crate's own: no second constant is invented here.
        let mut mic = mic(1_000).with_signal(0);
        mic.start().expect("start the stub microphone");
        let mut watch = ConvertedSilenceWatch::new(true); // as if the device needed conversion
        let mut warnings = 0usize;
        for (index, buffer) in mic.drain_for(CONVERTED_SILENCE_THRESHOLD).into_iter().enumerate() {
            assert!(is_silent_block(&buffer.data), "the stub was asked for digital silence");
            let warned = watch.observe(Duration::from_secs(1), is_silent_block(&buffer.data));
            warnings += usize::from(warned);
            let second = index as u64 + 1;
            if second < CONVERTED_SILENCE_THRESHOLD.as_secs() {
                assert!(!warned, "second {second} is still under the threshold");
            } else {
                assert!(warned, "the crossing second must warn");
            }
        }
        assert_eq!(warnings, 1, "once per session, not once per block");
        assert_eq!(mic.buffers_emitted(), CONVERTED_SILENCE_THRESHOLD.as_secs());
    }

    #[test]
    fn the_default_stub_microphone_never_trips_the_silence_watch() {
        // The other half of the contract: a stub that is not silent must keep the watch
        // disarmed even over more than the threshold's worth of blocks.
        let mut mic = mic(10);
        mic.start().expect("start the stub microphone");
        let mut watch = ConvertedSilenceWatch::new(true);
        for buffer in mic.drain_for(CONVERTED_SILENCE_THRESHOLD + Duration::from_secs(1)) {
            assert!(!is_silent_block(&buffer.data));
            assert!(
                !watch.observe(Duration::from_millis(10), is_silent_block(&buffer.data)),
                "real audio must never warn"
            );
        }
        assert!(!watch.has_warned());
    }

    #[test]
    fn next_buffer_with_zero_timeout_returns_none_when_not_due() {
        let mut mic = mic(10);
        mic.start().expect("start the stub microphone");
        // Block 0 is due immediately.
        assert!(mic.next_buffer(Duration::ZERO).expect("poll").is_some());
        // Block 1 is due 10ms later: a zero timeout must not block for it or invent it.
        assert!(
            mic.next_buffer(Duration::ZERO).expect("poll").is_none(),
            "must return None rather than block or fabricate a block"
        );
    }

    #[test]
    fn next_buffer_before_start_is_an_error() {
        let mut mic = mic(10);
        let err = mic
            .next_buffer(Duration::from_secs(1))
            .expect_err("the stub has no schedule before start");
        assert!(err.to_string().contains("not started"), "got: {err}");
    }

    #[test]
    fn the_microphone_backend_and_the_system_audio_backend_agree_on_the_canonical_format() {
        // Both audio tracks in the container are declared the same way (`-ar 48000 -ac 2`), and
        // the pipeline slices both by frame index, so a format mismatch between the microphone
        // backend and the system-audio backend is exactly what would slide the voice track
        // against the game track. Pin it.
        assert_eq!(
            MICROPHONE_FORMAT,
            AudioFormat::default(),
            "the microphone backend must publish the crate's canonical format"
        );

        #[cfg(windows)]
        {
            assert_eq!(
                crate::wasapi::TARGET,
                MICROPHONE_FORMAT,
                "the loopback backend and the microphone backend must target the same format"
            );
        }

        #[cfg(not(windows))]
        {
            // Off Windows the system-audio backend is the stub, and this is the one place on this
            // host where the two *selections* can be compared on live objects: both must hand out
            // the same format, frame count and byte count.
            let mut system = crate::platform::default_audio_backend(AudioFormat::default())
                .expect("the system-audio backend selection must succeed on this host");
            system.start().expect("start the system-audio backend");
            let system_block = system
                .next_buffer(Duration::from_secs(1))
                .expect("poll the system-audio backend")
                .expect("a block is due");
            let mut microphone = mic(10);
            microphone.start().expect("start the stub microphone");
            let mic_block = microphone
                .next_buffer(Duration::from_secs(1))
                .expect("poll the stub microphone")
                .expect("a block is due");
            assert_eq!(system_block.format, mic_block.format, "one canonical format, one track shape");
            assert_eq!(system_block.frames, mic_block.frames);
            assert_eq!(system_block.data.len(), mic_block.data.len());
        }
    }

    #[test]
    #[cfg(not(windows))]
    fn the_microphone_backend_is_unavailable_on_this_platform_with_a_clear_error() {
        // An Err, not an Ok and not a panic: a backend that silently succeeded here would make
        // the recorder believe it is capturing a voice track.
        let err = match microphone_backend() {
            Ok(_) => panic!("this host has no microphone backend: an Ok would be a silent success"),
            Err(e) => e,
        };
        let message = err.to_string();
        assert!(
            message.contains("only implemented on Windows"),
            "the error must say the microphone backend is Windows-only; got: {message}"
        );
    }
}
