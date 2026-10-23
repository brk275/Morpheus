use std::{
    ops::ControlFlow,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr, slice,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
        Arc,
    },
    time::Duration,
};

use windows::Win32::{
    Foundation::{self, PROPERTYKEY},
    Media::Audio,
    System::{Performance, Threading},
};

use crate::{
    host::{emit_error, equilibrium::fill_equilibrium, try_emit_error, ErrorCallbackArc},
    traits::StreamTrait,
    Data, Error, ErrorKind, FrameCount, InputCallbackInfo, InputStreamTimestamp,
    OutputCallbackInfo, OutputStreamTimestamp, ResultExt, SampleFormat, SampleRate, StreamConfig,
    StreamInstant,
};

/// Owns one unnamed auto-reset event from the moment creation succeeds.
///
/// This is handle ownership, not a WASAPI/native-disposal receipt. In particular,
/// the notification registration has a separate retirement contract.
pub(crate) struct OwnedEvent(OwnedHandle);

impl OwnedEvent {
    pub(crate) fn new() -> windows::core::Result<Self> {
        let event = unsafe { Threading::CreateEventW(None, false, false, None)? };
        // SAFETY: successful CreateEventW returns a new caller-owned, non-null
        // event HANDLE. No other OwnedHandle is constructed for this handle.
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(event.0) }))
    }

    pub(crate) fn signal(&self) -> windows::core::Result<()> {
        unsafe { Threading::SetEvent(self.handle()) }
    }

    pub(crate) fn handle(&self) -> Foundation::HANDLE {
        Foundation::HANDLE(self.0.as_raw_handle())
    }
}

/// Registration and both native references belong to the native stream STA.
/// The callback owns only copied flags and an actual shared event owner.
pub(crate) struct DefaultDeviceMonitor {
    enumerator: std::mem::ManuallyDrop<Audio::IMMDeviceEnumerator>,
    client: std::mem::ManuallyDrop<Audio::IMMNotificationClient>,
    event: Arc<OwnedEvent>,
    flow: Audio::EDataFlow,
    active: Arc<AtomicBool>,
    default_changed: Arc<AtomicBool>,
    unavailable: Arc<AtomicBool>,
    registered: bool,
}
impl DefaultDeviceMonitor {
    pub fn new(
        enumerator: Audio::IMMDeviceEnumerator,
        flow: Audio::EDataFlow,
        control: &super::owner::Control,
    ) -> Result<Self, Error> {
        let event = Arc::new(OwnedEvent::new().map_err(Error::from)?);
        let active = Arc::new(AtomicBool::new(true));
        let default_changed = Arc::new(AtomicBool::new(false));
        let unavailable = Arc::new(AtomicBool::new(false));
        let client: Audio::IMMNotificationClient = DefaultDeviceNotificationImpl {
            flow,
            event: event.clone(),
            active: active.clone(),
            default_changed: default_changed.clone(),
            unavailable: unavailable.clone(),
        }
        .into();
        let monitor = Self {
            enumerator: std::mem::ManuallyDrop::new(enumerator),
            client: std::mem::ManuallyDrop::new(client),
            event,
            flow,
            active,
            default_changed,
            unavailable,
            registered: true,
        };
        let status = unsafe {
            (windows::core::Interface::vtable(&*monitor.enumerator)
                .RegisterEndpointNotificationCallback)(
                windows::core::Interface::as_raw(&*monitor.enumerator),
                windows::core::Interface::as_raw(&*monitor.client),
            )
        };
        if status.0 != 0 {
            monitor.active.store(false, Ordering::Release);
            // Unknown registration output cannot justify destroying a possibly
            // retained callback. Keep this complete allocation on its STA.
            control.retain_failure(super::owner::WasapiCloseError::Notification);
        }
        Ok(monitor)
    }
    fn retire(&mut self, control: &super::owner::Control) {
        self.active.store(false, Ordering::Release);
        let status = unsafe {
            (windows::core::Interface::vtable(&*self.enumerator)
                .UnregisterEndpointNotificationCallback)(
                windows::core::Interface::as_raw(&*self.enumerator),
                windows::core::Interface::as_raw(&*self.client),
            )
        };
        if status.0 != 0 {
            control.retain_failure(super::owner::WasapiCloseError::Notification);
        }
        // Exact documented unregister permission outside callbacks, followed
        // by normal COM final Release, not a claim to join an OS callback thread.
        self.registered = false;
    }
    fn changed(&self) -> bool {
        if self.default_changed.swap(false, Ordering::AcqRel) {
            return true;
        }
        self.unavailable.swap(false, Ordering::AcqRel)
            && super::device::current_default_endpoint(self.flow).is_none()
    }
}
impl Drop for DefaultDeviceMonitor {
    fn drop(&mut self) {
        if self.registered {
            // Unexpected unwind cannot free a possibly registered callback.
            // Outer request catch retains the actual STA and reports failure.
            std::mem::forget(self.event.clone());
        } else {
            unsafe {
                std::mem::ManuallyDrop::drop(&mut self.client);
                std::mem::ManuallyDrop::drop(&mut self.enumerator);
            }
        }
    }
}
#[windows::core::implement(Audio::IMMNotificationClient)]
struct DefaultDeviceNotificationImpl {
    flow: Audio::EDataFlow,
    event: Arc<OwnedEvent>,
    active: Arc<AtomicBool>,
    default_changed: Arc<AtomicBool>,
    unavailable: Arc<AtomicBool>,
}
impl DefaultDeviceNotificationImpl {
    fn changed(&self, default: bool) -> windows::core::Result<()> {
        if self.active.load(Ordering::Acquire) {
            if default {
                self.default_changed.store(true, Ordering::Release);
            } else {
                self.unavailable.store(true, Ordering::Release);
            }
            self.event.signal()?;
        }
        Ok(())
    }
}
impl Audio::IMMNotificationClient_Impl for DefaultDeviceNotificationImpl_Impl {
    fn OnDefaultDeviceChanged(
        &self,
        flow: Audio::EDataFlow,
        role: Audio::ERole,
        _id: &windows::core::PCWSTR,
    ) -> windows::core::Result<()> {
        if flow == self.flow && role == Audio::eConsole {
            self.changed(true)?;
        }
        Ok(())
    }
    fn OnDeviceStateChanged(
        &self,
        _id: &windows::core::PCWSTR,
        state: Audio::DEVICE_STATE,
    ) -> windows::core::Result<()> {
        if state == Audio::DEVICE_STATE_DISABLED
            || state == Audio::DEVICE_STATE_NOTPRESENT
            || state == Audio::DEVICE_STATE_UNPLUGGED
        {
            self.changed(false)?;
        }
        Ok(())
    }
    fn OnDeviceAdded(&self, _id: &windows::core::PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, _id: &windows::core::PCWSTR) -> windows::core::Result<()> {
        self.changed(false)
    }
    fn OnPropertyValueChanged(
        &self,
        _id: &windows::core::PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

pub struct Stream {
    commands: std::sync::mpsc::SyncSender<Command>,
    pending_scheduled_event: Arc<OwnedEvent>,
    control: Option<super::owner::Control>,
    period_frames: FrameCount,
    qpc_frequency: u64,
    ready: Arc<AtomicBool>,
}
crate::assert_stream_send!(Stream);
crate::assert_stream_sync!(Stream);
struct RunContext {
    stream: StreamInner,
    handles: Vec<Foundation::HANDLE>,
    commands: Receiver<Command>,
    monitor: Option<DefaultDeviceMonitor>,
    control: super::owner::Control,
    _pending_scheduled_event: Arc<OwnedEvent>,
}

pub enum Command {
    PlayStream,
    PauseStream,
    Terminate,
}

pub enum AudioClientFlow {
    Render {
        render_client: Audio::IAudioRenderClient,
    },
    Capture {
        capture_client: Audio::IAudioCaptureClient,
    },
}

pub struct StreamInner {
    pub client_flow: AudioClientFlow,
    pub audio_clock: Audio::IAudioClock,
    pub audio_client: Audio::IAudioClient,
    pub(crate) cleanup_failure: std::cell::Cell<Option<super::owner::WasapiCloseError>>,
    // Event that is signalled by WASAPI whenever audio data must be written.
    pub(crate) event: OwnedEvent,
    // True if the stream is currently playing. False if paused.
    pub playing: bool,
    // Number of frames of audio data in the underlying buffer allocated by WASAPI.
    pub max_frames_in_buffer: FrameCount,
    // Callback size in frames.
    pub period_frames: FrameCount,
    // Number of bytes that each frame occupies.
    pub bytes_per_frame: u16,
    // The configuration with which the stream was created.
    pub config: StreamConfig,
    // The sample format with which the stream was created.
    pub sample_format: SampleFormat,
    // Hardware pipeline latency.
    pub stream_latency: Duration,
}

#[derive(Clone, Copy)]
struct BuildInfo {
    period_frames: FrameCount,
    qpc_frequency: u64,
}
impl Stream {
    pub(crate) fn new_input<D>(
        prepared: super::device::Prepared,
        config: StreamConfig,
        sample_format: SampleFormat,
        timeout: Option<Duration>,
        mut data_callback: D,
        error_callback: ErrorCallbackArc,
    ) -> Result<Self, Error>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
    {
        Self::build(
            prepared,
            move |device| device.build_input_stream_raw_inner(config, sample_format, timeout),
            move |context| run_input(context, &mut data_callback, &error_callback),
        )
    }
    pub(crate) fn new_output<D>(
        prepared: super::device::Prepared,
        config: StreamConfig,
        sample_format: SampleFormat,
        timeout: Option<Duration>,
        mut data_callback: D,
        error_callback: ErrorCallbackArc,
    ) -> Result<Self, Error>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
    {
        Self::build(
            prepared,
            move |device| device.build_output_stream_raw_inner(config, sample_format, timeout),
            move |context| run_output(context, &mut data_callback, &error_callback),
        )
    }
    fn build(
        prepared: super::device::Prepared,
        factory: impl FnOnce(&super::device::NativeDevice) -> Result<StreamInner, Error>
            + Send
            + 'static,
        run: impl FnOnce(&mut RunContext) + Send + 'static,
    ) -> Result<Self, Error> {
        let (commands, received) = std::sync::mpsc::sync_channel(1);
        let (published, observed) = std::sync::mpsc::sync_channel(1);
        let ready = Arc::new(AtomicBool::new(false));
        let ready_on_owner = ready.clone();
        let control = prepared.run(Box::new(move |device, control| {
            let inner = match factory(device) {
                Ok(inner) => inner,
                Err(error) => {
                    control.request_close();
                    let _ = published.send(Err(error));
                    return false;
                }
            };
            let monitor = match device.default_device_monitor() {
                Ok(monitor) => monitor,
                Err(error) => {
                    control.request_close();
                    let _ = published.send(Err(error));
                    return false;
                }
            };
            let mut frequency = 0;
            if unsafe { Performance::QueryPerformanceFrequency(&mut frequency) }.is_err()
                || frequency <= 0
            {
                // This is before Start; retire any registration explicitly.
                let mut monitor = monitor;
                if let Some(monitor) = monitor.as_mut() {
                    monitor.retire(control);
                }
                control.request_close();
                let _ = published.send(Err(Error::with_message(
                    ErrorKind::BackendError,
                    "Audio performance clock unavailable",
                )));
                return false;
            }
            let pending_event = control.event();
            let mut handles = vec![pending_event.handle(), inner.event.handle()];
            if let Some(monitor) = monitor.as_ref() {
                handles.push(monitor.event.handle());
            }
            let info = BuildInfo {
                period_frames: inner.period_frames,
                qpc_frequency: frequency as u64,
            };
            let mut context = std::mem::ManuallyDrop::new(RunContext {
                stream: inner,
                handles,
                commands: received,
                monitor,
                control: control.clone(),
                _pending_scheduled_event: pending_event,
            });
            if published.send(Ok(info)).is_err() {
                control.request_close();
            }
            while !ready_on_owner.load(Ordering::Acquire) && control.active() {
                if super::apartment::wait(&[control.event().handle()], Duration::from_millis(20))
                    .is_err()
                {
                    control.retain_failure(super::owner::WasapiCloseError::Apartment);
                }
            }
            if control.active() {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut context)))
                    .is_err()
                {
                    control.failure(super::owner::WasapiCloseError::WorkerPanicked);
                }
            }
            // Revocation precedes all native retirement. The owner-local
            // retention container survives every uncertain stop/unregister.
            control.request_close();
            if let Some(monitor) = context.monitor.as_ref() {
                monitor.active.store(false, Ordering::Release);
            }
            if let Some(failure) = context.stream.cleanup_failure.get() {
                control.retain_failure(failure);
            }
            let status = unsafe {
                (windows::core::Interface::vtable(&context.stream.audio_client).Stop)(
                    windows::core::Interface::as_raw(&context.stream.audio_client),
                )
            };
            if status.0 != 0 && status.0 != 1 {
                control.retain_failure(super::owner::WasapiCloseError::Stop);
            }
            context.stream.playing = false;
            if let Some(monitor) = context.monitor.as_mut() {
                monitor.retire(control);
            }
            // Declared field order releases services, clock and client before
            // audio event. No interface escapes this actual native worker.
            drop(unsafe { std::mem::ManuallyDrop::take(&mut context) });
            false
        }))?;
        let info = observed.recv().map_err(|_| {
            Error::with_message(
                ErrorKind::StreamInvalidated,
                "Audio construction did not complete",
            )
        })??;
        Ok(Self {
            commands,
            pending_scheduled_event: control.event(),
            control: Some(control),
            period_frames: info.period_frames,
            qpc_frequency: info.qpc_frequency,
            ready,
        })
    }
    pub(crate) fn signal_ready(&self) {
        self.ready.store(true, Ordering::Release);
        let _ = self.pending_scheduled_event.signal();
    }
    fn push_command(&self, command: Command) -> Result<(), Error> {
        if self
            .control
            .as_ref()
            .is_some_and(|control| !control.active())
        {
            return Err(ErrorKind::StreamInvalidated.into());
        }
        self.commands.try_send(command).map_err(|_| {
            Error::with_message(
                ErrorKind::DeviceBusy,
                "Audio command is pending or the owner has retired",
            )
        })?;
        self.pending_scheduled_event.signal().map_err(Error::from)
    }
    /// Nonblocking retirement request. Actual worker/TLS and supervisor joins,
    /// plus native/origin retirement, must be observed through this ticket.
    pub fn request_wasapi_close(self) -> super::owner::WasapiCloseTicket {
        let ticket = self.control.as_ref().expect("native Stream owner").ticket();
        ticket.request();
        drop(self);
        ticket
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        self.signal_ready();
        if let Some(control) = self.control.as_ref() {
            control.request_close();
        }
        // Compatibility signal for receivers, with independent atomic close
        // authority so a full command queue never blocks retirement.
        let _ = self.push_command(Command::Terminate);
    }
}

impl StreamTrait for Stream {
    fn play(&self) -> Result<(), Error> {
        self.push_command(Command::PlayStream)
    }

    fn pause(&self) -> Result<(), Error> {
        self.push_command(Command::PauseStream)
    }

    fn now(&self) -> StreamInstant {
        let mut counter: i64 = 0;
        unsafe {
            Performance::QueryPerformanceCounter(&mut counter)
                .expect("QueryPerformanceCounter failed");
        }
        // Convert to 100-nanosecond units first, matching the precision of WASAPI QPCPosition
        // values delivered to callbacks. This keeps `now()` on the same 100 ns grid as
        // callback/capture/playback instants, avoiding false sub-100 ns deltas.
        let units_100ns = counter as u128 * 10_000_000 / self.qpc_frequency as u128;
        let nanos = units_100ns * 100;
        StreamInstant::new(
            (nanos / 1_000_000_000) as u64,
            (nanos % 1_000_000_000) as u32,
        )
    }

    fn buffer_size(&self) -> Result<FrameCount, Error> {
        Ok(self.period_frames)
    }
}

// Process any pending commands that are queued within the `RunContext`.
// Returns `true` if the loop should continue running, `false` if it should terminate.
fn process_commands(run_context: &mut RunContext) -> Result<bool, Error> {
    // Independent close/account authority wins over even an already queued
    // Play. No future Start is authorized by a stale command-channel entry.
    if !run_context.control.active() {
        return Ok(false);
    }
    for command in run_context.commands.try_iter() {
        if !run_context.control.active() {
            return Ok(false);
        }
        match command {
            Command::PlayStream => unsafe {
                if !run_context.stream.playing {
                    run_context
                        .stream
                        .audio_client
                        .Start()
                        .context("Failed to start audio client")?;
                    run_context.stream.playing = true;
                }
            },
            Command::PauseStream => unsafe {
                if run_context.stream.playing {
                    let status =
                        (windows::core::Interface::vtable(&run_context.stream.audio_client).Stop)(
                            windows::core::Interface::as_raw(&run_context.stream.audio_client),
                        );
                    if status.0 != 0 && status.0 != 1 {
                        run_context
                            .stream
                            .cleanup_failure
                            .set(Some(super::owner::WasapiCloseError::Stop));
                        return Err(Error::with_message(
                            ErrorKind::StreamInvalidated,
                            "Audio stop could not be confirmed",
                        ));
                    }
                    run_context.stream.playing = false;
                }
            },
            Command::Terminate => {
                return Ok(false);
            }
        }
    }

    Ok(true)
}
// Wait for any of the given handles to be signalled.
//
// Returns the index of the `handle` that was signalled, or an `Err` if
// `WaitForMultipleObjectsEx` fails.
//
// This is called when the `run` thread is ready to wait for the next event. The
// next event might be some command submitted by the user (the first handle) or
// might indicate that one of the streams is ready to deliver or receive audio.
fn wait_for_handle_signal(handles: &[Foundation::HANDLE]) -> Result<usize, Error> {
    loop {
        if let Some(index) = super::apartment::wait(handles, Duration::from_millis(50))
            .map_err(|kind| Error::with_message(ErrorKind::StreamInvalidated, kind.to_string()))?
        {
            return Ok(index);
        }
    }
}

// Get the number of available frames that are available for writing/reading.
#[inline]
fn get_available_frames(stream: &StreamInner) -> Result<FrameCount, Error> {
    unsafe {
        let padding = stream
            .audio_client
            .GetCurrentPadding()
            .context("Failed to get current padding")?;
        Ok(stream.max_frames_in_buffer - padding)
    }
}

// One initialized, aligned native-owner scratch allocation handles silent and
// 24-in-32 input packets. Native metadata cannot request an unbounded buffer.
const MAX_CAPTURE_PACKET_BYTES: usize = 64 * 1024 * 1024;
struct CaptureScratch {
    words: Vec<u64>,
    maximum_frames: FrameCount,
    bytes_per_frame: usize,
    sample_format: SampleFormat,
}
impl CaptureScratch {
    fn new(
        maximum_frames: FrameCount,
        bytes_per_frame: u16,
        sample_format: SampleFormat,
    ) -> Result<Self, Error> {
        let bytes_per_frame = usize::from(bytes_per_frame);
        let supported = matches!(
            sample_format,
            SampleFormat::U8
                | SampleFormat::I16
                | SampleFormat::I24
                | SampleFormat::I32
                | SampleFormat::I64
                | SampleFormat::F32
                | SampleFormat::F64
        );
        let byte_count = (maximum_frames as usize)
            .checked_mul(bytes_per_frame)
            .filter(|bytes| *bytes > 0 && *bytes <= MAX_CAPTURE_PACKET_BYTES)
            .ok_or_else(|| Self::invalid("Capture buffer exceeds the owned packet bound"))?;
        if !supported || bytes_per_frame == 0 || bytes_per_frame % sample_format.sample_size() != 0
        {
            return Err(Self::invalid("Capture frame format is not supported"));
        }
        let word_count = byte_count.div_ceil(size_of::<u64>());
        let mut words = Vec::new();
        words.try_reserve_exact(word_count).map_err(|_| {
            Error::with_message(ErrorKind::ResourceExhausted, "Capture scratch unavailable")
        })?;
        words.resize(word_count, 0);
        Ok(Self {
            words,
            maximum_frames,
            bytes_per_frame,
            sample_format,
        })
    }
    fn invalid(message: &'static str) -> Error {
        Error::with_message(ErrorKind::StreamInvalidated, message)
    }
    /// The caller retains the nonzero GetBuffer acquisition through callback
    /// completion and keeps this scratch alive and unchanged while Data is used.
    /// A non-silent pointer must address the negotiated valid native packet.
    unsafe fn packet(
        &mut self,
        native: *const u8,
        frames: FrameCount,
        flags: u32,
    ) -> Result<Option<Data>, Error> {
        // AUDCLNT_S_BUFFER_EMPTY is a successful HRESULT whose data and
        // timestamp outputs are untouched. Never inspect them or form Data.
        if frames == 0 {
            return Ok(None);
        }
        if frames > self.maximum_frames {
            return Err(Self::invalid(
                "Capture packet exceeds the negotiated buffer",
            ));
        }
        let byte_count = (frames as usize)
            .checked_mul(self.bytes_per_frame)
            .filter(|bytes| *bytes <= MAX_CAPTURE_PACKET_BYTES)
            .ok_or_else(|| Self::invalid("Capture packet exceeds the owned packet bound"))?;
        let samples = byte_count / self.sample_format.sample_size();
        let data = if flags & Audio::AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
            // SILENT means ignore native payload, including a null or
            // unaligned value. u64 storage provides all admitted alignments.
            let owned = slice::from_raw_parts_mut(self.words.as_mut_ptr().cast::<u8>(), byte_count);
            fill_equilibrium(owned, self.sample_format);
            self.words.as_mut_ptr().cast()
        } else {
            if native.is_null() || (native as usize) % self.sample_format.sample_size() != 0 {
                return Err(Self::invalid("Capture packet data is not aligned"));
            }
            if self.sample_format == SampleFormat::I24 {
                // Preserve WASAPI's signed 24-in-32 arithmetic conversion.
                let source = slice::from_raw_parts(native.cast::<i32>(), samples);
                let owned =
                    slice::from_raw_parts_mut(self.words.as_mut_ptr().cast::<i32>(), samples);
                for (destination, source) in owned.iter_mut().zip(source) {
                    *destination = *source >> 8;
                }
                self.words.as_mut_ptr().cast()
            } else {
                native.cast_mut().cast()
            }
        };
        Ok(Some(Data::from_parts(data, samples, self.sample_format)))
    }
}

fn run_input(
    run_ctxt: &mut RunContext,
    data_callback: &mut dyn FnMut(&Data, &InputCallbackInfo),
    error_callback: &ErrorCallbackArc,
) {
    #[cfg(feature = "realtime")]
    if let Err(err) = boost_current_thread_priority(
        run_ctxt.stream.period_frames,
        run_ctxt.stream.config.sample_rate,
    ) {
        emit_error(error_callback, err);
    }

    let stream = &run_ctxt.stream;
    let mut scratch = match CaptureScratch::new(
        stream.max_frames_in_buffer,
        stream.bytes_per_frame,
        stream.sample_format,
    ) {
        Ok(scratch) => scratch,
        Err(error) => {
            emit_error(error_callback, error);
            return;
        }
    };

    loop {
        match process_commands_and_await_signal(run_ctxt, error_callback) {
            ControlFlow::Break(()) => break,
            ControlFlow::Continue(false) => continue,
            ControlFlow::Continue(true) => {}
        }
        if !run_ctxt.control.active() {
            break;
        }
        let capture_client = match run_ctxt.stream.client_flow {
            AudioClientFlow::Capture { ref capture_client } => capture_client.clone(),
            _ => unreachable!(),
        };
        if let Err(err) = process_input(
            &run_ctxt.stream,
            &run_ctxt.control,
            capture_client,
            data_callback,
            error_callback,
            &mut scratch,
        ) {
            emit_error(error_callback, err);
            break;
        }
    }
}

fn run_output(
    run_ctxt: &mut RunContext,
    data_callback: &mut dyn FnMut(&mut Data, &OutputCallbackInfo),
    error_callback: &ErrorCallbackArc,
) {
    #[cfg(feature = "realtime")]
    if let Err(err) = boost_current_thread_priority(
        run_ctxt.stream.period_frames,
        run_ctxt.stream.config.sample_rate,
    ) {
        emit_error(error_callback, err);
    }

    // The clock frequency is constant for the stream's lifetime.
    let clock_frequency = match unsafe { run_ctxt.stream.audio_clock.GetFrequency() }
        .context("Failed to get audio clock frequency")
    {
        Ok(0) => {
            emit_error(
                error_callback,
                Error::with_message(
                    ErrorKind::BackendError,
                    "IAudioClock::GetFrequency returned zero",
                ),
            );
            return;
        }
        Ok(frequency) => frequency,
        Err(err) => {
            emit_error(error_callback, err);
            return;
        }
    };
    let mut frames_written: u64 = 0;

    loop {
        match process_commands_and_await_signal(run_ctxt, error_callback) {
            ControlFlow::Break(()) => break,
            ControlFlow::Continue(false) => continue,
            ControlFlow::Continue(true) => {}
        }
        if !run_ctxt.control.active() {
            break;
        }
        let render_client = match run_ctxt.stream.client_flow {
            AudioClientFlow::Render { ref render_client } => render_client.clone(),
            _ => unreachable!(),
        };
        if let Err(err) = process_output(
            &run_ctxt.stream,
            &run_ctxt.control,
            render_client,
            data_callback,
            clock_frequency,
            &mut frames_written,
        ) {
            emit_error(error_callback, err);
            break;
        }
    }
}

/// Attempts to elevate the current thread to real-time or high-priority scheduling.
#[cfg(feature = "realtime")]
fn boost_current_thread_priority(
    period_frames: FrameCount,
    sample_rate: SampleRate,
) -> Result<(), Error> {
    match audio_thread_priority::promote_current_thread_to_real_time(period_frames, sample_rate) {
        Ok(_) => Ok(()),
        Err(_) => unsafe {
            let thread_handle = Threading::GetCurrentThread();
            Threading::SetThreadPriority(thread_handle, Threading::THREAD_PRIORITY_TIME_CRITICAL)
                .context("Failed to promote audio thread to real-time priority")
        },
    }
}

// WASAPI never rebinds the IAudioClient, so report what's actually true instead of DeviceChanged.
fn default_device_change_error(flow: Option<Audio::EDataFlow>) -> Error {
    match flow.and_then(super::device::current_default_endpoint) {
        None => ErrorKind::DeviceNotAvailable.into(),
        Some(_) => ErrorKind::StreamInvalidated.into(),
    }
}

fn process_commands_and_await_signal(
    run_context: &mut RunContext,
    error_callback: &ErrorCallbackArc,
) -> ControlFlow<(), bool> {
    // Process queued commands.
    match process_commands(run_context) {
        Ok(true) => (),
        Ok(false) => return ControlFlow::Break(()),
        Err(err) => {
            emit_error(error_callback, err);
            return ControlFlow::Break(());
        }
    };

    if !run_context.control.active() {
        return ControlFlow::Break(());
    }
    if let Some(monitor) = run_context.monitor.as_ref() {
        if monitor.changed() {
            emit_error(
                error_callback,
                default_device_change_error(Some(monitor.flow)),
            );
        }
    }

    // Wait for any of the handles to be signalled.
    let handle_idx = match wait_for_handle_signal(&run_context.handles) {
        Ok(idx) => idx,
        Err(err) => {
            emit_error(error_callback, err);
            return ControlFlow::Break(());
        }
    };

    // Handle layout: 0 = pending_scheduled_event (commands), 1 = WASAPI audio event,
    // 2+ = default-device change event (only present for default-device streams).
    // Continue(true)  = audio event fired, proceed to process audio this iteration.
    // Continue(false) = command or device-change event, loop around and wait again.
    if handle_idx >= 2 {
        if let Some(monitor) = run_context.monitor.as_ref() {
            if monitor.changed() {
                emit_error(
                    error_callback,
                    default_device_change_error(Some(monitor.flow)),
                );
            }
        }
        return ControlFlow::Continue(false);
    }
    ControlFlow::Continue(handle_idx != 0)
}

struct CaptureBuffer<'a> {
    client: &'a Audio::IAudioCaptureClient,
    frames: u32,
    released: bool,
    failed: &'a std::cell::Cell<Option<super::owner::WasapiCloseError>>,
}
impl CaptureBuffer<'_> {
    fn release(&mut self) -> Result<(), Error> {
        self.released = true; // no uncertain native retry in Drop
        let status = unsafe {
            (windows::core::Interface::vtable(self.client).ReleaseBuffer)(
                windows::core::Interface::as_raw(self.client),
                self.frames,
            )
        };
        if status.0 != 0 {
            self.failed
                .set(Some(super::owner::WasapiCloseError::BufferRelease));
            return Err(Error::with_message(
                ErrorKind::StreamInvalidated,
                "Capture buffer retirement could not be confirmed",
            ));
        }
        Ok(())
    }
}
impl Drop for CaptureBuffer<'_> {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.release();
        }
    }
}
struct RenderBuffer<'a> {
    client: &'a Audio::IAudioRenderClient,
    frames: u32,
    released: bool,
    failed: &'a std::cell::Cell<Option<super::owner::WasapiCloseError>>,
}
impl RenderBuffer<'_> {
    fn release(&mut self) -> Result<(), Error> {
        self.released = true;
        let status = unsafe {
            (windows::core::Interface::vtable(self.client).ReleaseBuffer)(
                windows::core::Interface::as_raw(self.client),
                self.frames,
                0,
            )
        };
        if status.0 != 0 {
            self.failed
                .set(Some(super::owner::WasapiCloseError::BufferRelease));
            return Err(Error::with_message(
                ErrorKind::StreamInvalidated,
                "Render buffer retirement could not be confirmed",
            ));
        }
        Ok(())
    }
}
impl Drop for RenderBuffer<'_> {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.release();
        }
    }
}

// The loop for processing pending input data.
fn process_input(
    stream: &StreamInner,
    control: &super::owner::Control,
    capture_client: Audio::IAudioCaptureClient,
    data_callback: &mut dyn FnMut(&Data, &InputCallbackInfo),
    error_callback: &ErrorCallbackArc,
    scratch: &mut CaptureScratch,
) -> Result<(), Error> {
    unsafe {
        // Get the available data in the shared buffer.
        loop {
            if !control.active() {
                return Ok(());
            }
            let mut frames_available = match capture_client.GetNextPacketSize() {
                Ok(0) => return Ok(()),
                Ok(f) => f,
                Err(err) => return Err(Error::from(err)),
            };
            let mut buffer: *mut u8 = ptr::null_mut();
            let mut flags = 0;
            let mut qpc_position: u64 = 0;
            let mut device_position: u64 = 0;
            let result = capture_client.GetBuffer(
                &mut buffer,
                &mut frames_available,
                &mut flags,
                Some(&mut device_position),
                Some(&mut qpc_position),
            );

            result.map_err(Error::from)?;
            // The wrapper maps all successful HRESULTs to Ok, including
            // AUDCLNT_S_BUFFER_EMPTY. For zero frames the SDK permits no
            // ReleaseBuffer; data/timestamp outputs must not be admitted.
            if frames_available == 0 {
                return Ok(());
            }

            let mut acquired = CaptureBuffer {
                client: &capture_client,
                frames: frames_available,
                released: false,
                failed: &stream.cleanup_failure,
            };
            // The discontinuity flag is undefined on the first GetBuffer after Start,
            // where device_position is still 0.
            if device_position != 0
                && flags & Audio::AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0
            {
                let _ = try_emit_error(error_callback, ErrorKind::Xrun.into());
            }

            // Every nonzero packet is already guarded: invalid metadata,
            // timestamp errors and callback unwinds still release it once.
            let data = scratch
                .packet(buffer, frames_available, flags)?
                .ok_or_else(|| CaptureScratch::invalid("Capture packet unexpectedly empty"))?;

            // The `qpc_position` is in 100 nanosecond units. Convert it to nanoseconds.
            let timestamp = input_timestamp(stream, qpc_position)?;
            let info = InputCallbackInfo { timestamp };
            if control.active() {
                data_callback(&data, &info);
            }

            // Release the buffer.
            acquired.release()?;
        }
    }
}

// The loop for writing output data.
fn process_output(
    stream: &StreamInner,
    control: &super::owner::Control,
    render_client: Audio::IAudioRenderClient,
    data_callback: &mut dyn FnMut(&mut Data, &OutputCallbackInfo),
    clock_frequency: u64,
    frames_written: &mut u64,
) -> Result<(), Error> {
    // The number of frames available for writing.
    let frames_available = match get_available_frames(stream)? {
        0 => return Ok(()), // TODO: Can this happen?
        n => n,
    };

    unsafe {
        let buffer = render_client.GetBuffer(frames_available)?;
        let mut acquired = RenderBuffer {
            client: &render_client,
            frames: frames_available,
            released: false,
            failed: &stream.cleanup_failure,
        };

        debug_assert!(!buffer.is_null());

        let byte_count = frames_available as usize * stream.bytes_per_frame as usize;
        let buffer_slice = std::slice::from_raw_parts_mut(buffer, byte_count);
        fill_equilibrium(buffer_slice, stream.sample_format);

        let data = buffer as *mut ();
        let len = byte_count / stream.sample_format.sample_size();
        let mut data = Data::from_parts(data, len, stream.sample_format);
        let sample_rate = stream.config.sample_rate;
        let timestamp = output_timestamp(stream, sample_rate, clock_frequency, *frames_written)?;
        let info = OutputCallbackInfo { timestamp };
        if control.active() {
            data_callback(&mut data, &info);
        }

        if stream.sample_format == SampleFormat::I24 {
            // WASAPI stores i24 in the upper bits
            #[expect(
                clippy::cast_ptr_alignment,
                reason = "WASAPI guarantees the buffer to be aligned to a frame boundary"
            )]
            let buffer_slice_i32 = slice::from_raw_parts_mut(buffer.cast::<i32>(), len);
            for sample in buffer_slice_i32 {
                *sample <<= 8;
            }
        }

        acquired.release()?;

        *frames_written += frames_available as u64;
    }

    Ok(())
}

/// Reads the stream's `IAudioClock` in a single `GetPosition` call, returning the callback
/// [`StreamInstant`] together with the device position from that same snapshot.
#[inline]
fn clock_position(stream: &StreamInner) -> Result<(StreamInstant, u64), Error> {
    let mut position: u64 = 0;
    let mut qpc_position: u64 = 0;
    unsafe {
        stream
            .audio_clock
            .GetPosition(&mut position, Some(&mut qpc_position))
            .context("Failed to get clock position")?;
    };
    // The `qpc_position` is in 100-nanosecond units.
    let nanos = qpc_position as u128 * 100;
    let instant = StreamInstant::new(
        (nanos / 1_000_000_000) as u64,
        (nanos % 1_000_000_000) as u32,
    );
    Ok((instant, position))
}

/// Produce the input stream timestamp.
///
/// `buffer_qpc_position` is the `qpc_position` returned via the `GetBuffer` call on the capture
/// client. It represents the instant at which the first sample of the retrieved buffer was
/// captured.
#[inline]
fn input_timestamp(
    stream: &StreamInner,
    buffer_qpc_position: u64,
) -> Result<InputStreamTimestamp, Error> {
    // The `qpc_position` is in 100-nanosecond units.
    let nanos = buffer_qpc_position as u128 * 100;
    let capture = StreamInstant::new(
        (nanos / 1_000_000_000) as u64,
        (nanos % 1_000_000_000) as u32,
    );
    let (callback, _position) = clock_position(stream)?;
    Ok(InputStreamTimestamp { capture, callback })
}

/// Produce the output stream timestamp.
///
/// `sample_rate` is the rate at which audio frames are processed by the device.
///
/// `clock_frequency` is the device clock's constant tick rate, used to convert the reported clock
/// position into a played-out duration.
///
/// `frames_written` is the running total of frames submitted to the render buffer so far, used to
/// derive how much audio is buffered ahead of the device position.
#[inline]
fn output_timestamp(
    stream: &StreamInner,
    sample_rate: SampleRate,
    clock_frequency: u64,
    frames_written: u64,
) -> Result<OutputStreamTimestamp, Error> {
    let (callback, position) = clock_position(stream)?;
    // `buffered` is the amount of audio we've already submitted that has not yet been consumed by
    // the device at this instant; it determines when the next written frame will be heard.
    let consumed_nanos = position as u128 * 1_000_000_000 / clock_frequency as u128;
    let written_nanos = frames_written as u128 * 1_000_000_000 / sample_rate as u128;
    let buffered_nanos =
        u64::try_from(written_nanos.saturating_sub(consumed_nanos)).unwrap_or(u64::MAX);
    let buffered = Duration::from_nanos(buffered_nanos);

    let playback = callback + (buffered + stream.stream_latency);
    Ok(OutputStreamTimestamp { callback, playback })
}

#[cfg(test)]
mod tests {
    use super::{Command, OwnedEvent, Stream};
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::{sync_channel, Receiver};
    use std::sync::{Arc, Weak};
    use windows::Win32::{
        Foundation::WAIT_OBJECT_0,
        System::Threading::{SetEvent, WaitForSingleObject},
    };

    // No device, COM object or audio thread exists in this fixture. It keeps
    // the real Stream command sender/latch/drop path and a real kernel event.
    fn command_stream_fixture(
    ) -> windows::core::Result<(Stream, Receiver<Command>, Arc<OwnedEvent>)> {
        let pending_scheduled_event = Arc::new(OwnedEvent::new()?);
        let worker_owner = Arc::clone(&pending_scheduled_event);
        let (commands, receiver) = sync_channel(1);
        Ok((
            Stream {
                commands,
                pending_scheduled_event,
                period_frames: 1,
                qpc_frequency: 1,
                control: None,
                ready: Arc::new(AtomicBool::new(false)),
            },
            receiver,
            worker_owner,
        ))
    }

    #[test]
    fn capture_empty_success_never_admits_data_and_bounds_nonempty_packets() {
        use super::CaptureScratch;
        use crate::SampleFormat;
        let mut scratch = CaptureScratch::new(2, 4, SampleFormat::F32).unwrap();
        assert!(windows::Win32::Media::Audio::AUDCLNT_S_BUFFER_EMPTY
            .ok()
            .is_ok());
        // Deliberately unusable pointer/flags: an empty success has no packet.
        let empty = unsafe { scratch.packet(std::ptr::null(), 0, u32::MAX) }.unwrap();
        assert!(empty.is_none());
        assert!(unsafe { scratch.packet(std::ptr::null(), 3, 0) }.is_err());
        assert!(unsafe { scratch.packet(std::ptr::null(), 1, 0) }.is_err());
        let aligned = [0u64; 1];
        let unaligned = unsafe { aligned.as_ptr().cast::<u8>().add(1) };
        assert!(unsafe { scratch.packet(unaligned, 1, 0) }.is_err());
        assert!(CaptureScratch::new(u32::MAX, u16::MAX, SampleFormat::F64).is_err());
        assert!(CaptureScratch::new(1, 3, SampleFormat::F32).is_err());
    }

    #[test]
    fn capture_silence_uses_owned_equilibrium_and_keeps_signed_24_conversion() {
        use super::CaptureScratch;
        use crate::SampleFormat;
        let silent = windows::Win32::Media::Audio::AUDCLNT_BUFFERFLAGS_SILENT.0 as u32;
        for format in [
            SampleFormat::U8,
            SampleFormat::I16,
            SampleFormat::I24,
            SampleFormat::I32,
            SampleFormat::I64,
            SampleFormat::F32,
            SampleFormat::F64,
        ] {
            let size = format.sample_size();
            let mut scratch = CaptureScratch::new(2, size as u16, format).unwrap();
            // Dirty the real owned storage first, so zero initialization alone
            // cannot masquerade as per-packet SILENT handling.
            scratch.words.fill(u64::MAX);
            let data = unsafe { scratch.packet(std::ptr::null(), 2, silent) }
                .unwrap()
                .unwrap();
            let equilibrium = if format == SampleFormat::U8 { 0x80 } else { 0 };
            assert_eq!(data.bytes(), vec![equilibrium; 2 * size]);
            let pointer = data.bytes().as_ptr() as usize;
            assert_eq!(pointer % size, 0);
            let mut arbitrary = [0x7fu8; 17];
            let data = unsafe { scratch.packet(arbitrary.as_mut_ptr().add(1), 2, silent) }
                .unwrap()
                .unwrap();
            assert_eq!(data.bytes(), vec![equilibrium; 2 * size]);
            assert_eq!(arbitrary, [0x7f; 17]);
        }
        let native = [i32::MIN, -256, 0, 0x7fff_ff00];
        let mut scratch = CaptureScratch::new(4, 4, SampleFormat::I24).unwrap();
        let data = unsafe { scratch.packet(native.as_ptr().cast(), 4, 0) }
            .unwrap()
            .unwrap();
        let converted: Vec<i32> = data
            .bytes()
            .chunks_exact(4)
            .map(|sample| i32::from_ne_bytes(sample.try_into().unwrap()))
            .collect();
        assert_eq!(converted, vec![-8_388_608, -1, 0, 8_388_607]);
        assert_eq!(native, [i32::MIN, -256, 0, 0x7fff_ff00]);
        let samples = [0x1234i16, -2];
        let mut scratch = CaptureScratch::new(2, 2, SampleFormat::I16).unwrap();
        // Timestamp uncertainty does not invalidate otherwise valid samples;
        // this decoder does not construct or certify a timestamp.
        let data = unsafe {
            scratch.packet(
                samples.as_ptr().cast(),
                2,
                windows::Win32::Media::Audio::AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32,
            )
        }
        .unwrap()
        .unwrap();
        assert_eq!(data.bytes().as_ptr(), samples.as_ptr().cast::<u8>());
        assert_eq!(data.bytes(), unsafe {
            std::slice::from_raw_parts(samples.as_ptr().cast::<u8>(), 4)
        });
    }

    #[test]
    fn command_event_caller_can_signal_after_worker_owner_retires() {
        let (stream, receiver, worker_owner) =
            command_stream_fixture().expect("create synthetic command Stream");
        drop(worker_owner);
        // Keep Receiver live so send succeeds even though the worker-side
        // event owner has retired. Exercise the actual Stream signaling path.
        let observed = stream.push_command(Command::PlayStream).map(|()| {
            let command = receiver.try_recv();
            let signaled =
                unsafe { WaitForSingleObject(stream.pending_scheduled_event.handle(), 0) };
            (command, signaled)
        });
        drop(stream);
        drop(receiver);
        let (command, signaled) = observed.expect("signal retained Stream event");
        assert!(matches!(command, Ok(Command::PlayStream)));
        assert_eq!(signaled, WAIT_OBJECT_0);
    }

    #[test]
    fn command_event_worker_can_wait_after_caller_owner_retires() {
        let (stream, receiver, worker_owner) =
            command_stream_fixture().expect("create synthetic command Stream");
        // Exercise actual Stream::drop, including its Terminate send/SetEvent.
        // The worker-side owner must keep that exact event alive afterward.
        drop(stream);
        let command = receiver.try_recv();
        let signaled = unsafe { WaitForSingleObject(worker_owner.handle(), 0) };
        drop(worker_owner);
        drop(receiver);
        assert!(matches!(command, Ok(Command::Terminate)));
        assert_eq!(signaled, WAIT_OBJECT_0);
    }

    #[test]
    fn owned_event_is_not_retained_by_a_failed_later_construction_step() {
        let mut retired = Weak::<OwnedEvent>::new();
        let observed = (|| -> Result<(), &'static str> {
            let event = Arc::new(OwnedEvent::new().map_err(|_| "event creation failed")?);
            retired = Arc::downgrade(&event);
            unsafe { SetEvent(event.handle()) }.map_err(|_| "event signaling failed")?;
            Err("later setup failed")
        })();
        assert_eq!(observed, Err("later setup failed"));
        // OwnedHandle performs the matching CloseHandle when this final Rust
        // owner is destroyed. Never probe a stale raw HANDLE: another process
        // thread could already have reused its value. This is not a native
        // WASAPI disposal receipt.
        assert!(retired.upgrade().is_none());
    }

    #[test]
    fn owned_event_is_not_retained_by_a_construction_unwind() {
        let mut retired = Weak::<OwnedEvent>::new();
        let mut created = false;
        let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let event = Arc::new(OwnedEvent::new().expect("create unnamed command event"));
            retired = Arc::downgrade(&event);
            created = true;
            panic!("synthetic later event-construction failure");
        }));
        assert!(created);
        assert!(observed.is_err());
        assert!(retired.upgrade().is_none());
    }
}
