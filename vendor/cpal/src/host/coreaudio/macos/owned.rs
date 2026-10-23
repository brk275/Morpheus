//! Fallible owned CoreAudio control and shutdown. No native owner is released by Drop.
use super::{
    callback_gate::{self, Body, RawArgs, Retired, Token},
    device::{self, Device},
    loopback::LoopbackDevice,
};
use crate::{Error, ErrorKind, SampleFormat, StreamConfig};
use coreaudio::audio_unit::{Element, Scope};
use objc2_audio_toolbox::{
    kAudioOutputUnitProperty_SetInputCallback, kAudioUnitManufacturer_Apple,
    kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitProperty_SetRenderCallback,
    kAudioUnitSubType_DefaultOutput, kAudioUnitSubType_HALOutput, kAudioUnitType_Output,
    AURenderCallbackStruct, AudioComponentDescription, AudioComponentFindNext,
    AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioOutputUnitStart,
    AudioOutputUnitStop, AudioUnit as NativeUnit, AudioUnitGetProperty, AudioUnitInitialize,
    AudioUnitSetProperty, AudioUnitUninitialize,
};
use objc2_core_audio::{
    kAudioDeviceProcessorOverload, kAudioDevicePropertyDeviceIsAlive,
    kAudioDevicePropertyNominalSampleRate, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    AudioObjectAddPropertyListener, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectRemovePropertyListener,
};
use std::{
    any::Any,
    cell::RefCell,
    panic::{catch_unwind, AssertUnwindSafe},
    ptr::NonNull,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
        mpsc, Arc, Condvar, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const OWNERS: usize = 16;
const DEADLINE: Duration = Duration::from_secs(5);
const ALIVE: u32 = 1;
const RATE: u32 = 2;
const OVERLOAD: u32 = 4;
const DEFAULT: u32 = 8;

fn error(message: &'static str) -> Error {
    Error::with_message(ErrorKind::StreamInvalidated, message)
}
fn status(value: i32) -> Result<(), Error> {
    super::check_os_status(value)
}
fn reentrant() -> bool {
    callback_gate::is_callback_thread() || {
        let current = unsafe { libc::pthread_self() };
        registry().lock().map_or(true, |owners| {
            owners.iter().flatten().any(|o| {
                let owner = o.owner_thread.load(Ordering::Acquire);
                owner != 0 && current as usize == owner
            })
        })
    }
}

/// Exact fixed phases; native numeric status carries no endpoint, account or error text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoreAudioClosePhase {
    CallbackGate,
    ListenerRemove,
    Stop,
    Uninitialize,
    Dispose,
    AggregateDestroy,
    TapDestroy,
    CallbackRelease,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnedCoreAudioCloseError {
    Pending,
    Reentrant,
    Ownership,
    OwnerPanicked,
    Native {
        phase: CoreAudioClosePhase,
        status: i32,
    },
}
impl std::fmt::Display for OwnedCoreAudioCloseError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Pending => "CoreAudio native close remains owned",
            Self::Reentrant => "CoreAudio reentrant close refused",
            Self::Ownership => "CoreAudio close ownership could not be confirmed",
            Self::OwnerPanicked => "CoreAudio owner or TLS panicked",
            Self::Native { .. } => "CoreAudio native cleanup failed and remains owned",
        })
    }
}
impl std::error::Error for OwnedCoreAudioCloseError {}
type CloseFailure = OwnedCoreAudioCloseError;
#[derive(Default)]
struct Completion {
    attempt: usize,
    failure: Option<CloseFailure>,
    native_finished: bool,
    joined: bool,
}
enum Command {
    Play(mpsc::Sender<Result<(), Error>>),
    Pause(mpsc::Sender<Result<(), Error>>),
    Buffer(mpsc::Sender<Result<u32, Error>>),
}
struct Entry {
    owner_thread: AtomicUsize,
    closing: AtomicBool,
    callback_tokens: [AtomicUsize; 16],
    requests: AtomicUsize,
    commands: mpsc::SyncSender<Command>,
    completion: Mutex<Completion>,
    changed: Condvar,
    // The slot retains this handle BEFORE its worker can construct native resources.
    join: Mutex<Option<JoinHandle<()>>>,
    join_panic: Mutex<Option<Box<dyn Any + Send>>>,
    join_serial: Mutex<()>,
}
impl Entry {
    fn request_close(&self) -> usize {
        self.closing.store(true, Ordering::SeqCst);
        for token in &self.callback_tokens {
            callback_gate::revoke_exact(token.load(Ordering::SeqCst));
        }
        self.requests
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .unwrap_or(usize::MAX)
    }
}
static REGISTRY: OnceLock<Mutex<Vec<Option<Arc<Entry>>>>> = OnceLock::new();
fn registry() -> &'static Mutex<Vec<Option<Arc<Entry>>>> {
    REGISTRY.get_or_init(|| Mutex::new((0..OWNERS).map(|_| None).collect()))
}

pub(super) struct Stream {
    entry: Arc<Entry>,
    index: usize,
}
impl Stream {
    /// Every builder runs on its already retained owner; Resources captures partial construction.
    pub fn build<F>(build: F) -> Result<Self, Error>
    where
        F: FnOnce(&mut Resources) -> Result<Setup, Error> + Send + 'static,
    {
        if reentrant() {
            return Err(error("CoreAudio reentrant construction refused"));
        }
        callback_gate::prewarm()?;
        let (tx, rx) = mpsc::sync_channel(16);
        let entry = Arc::new(Entry {
            owner_thread: AtomicUsize::new(0),
            closing: AtomicBool::new(false),
            callback_tokens: [const { AtomicUsize::new(0) }; 16],
            requests: AtomicUsize::new(0),
            commands: tx,
            completion: Mutex::new(Completion::default()),
            changed: Condvar::new(),
            join: Mutex::new(None),
            join_panic: Mutex::new(None),
            join_serial: Mutex::new(()),
        });
        let (start_tx, start_rx) = mpsc::sync_channel(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let mut owners = registry()
            .lock()
            .map_err(|_| error("CoreAudio owner registry unavailable"))?;
        // A failed or pending close blocks replacement, rather than accumulating a new device.
        if owners
            .iter()
            .flatten()
            .any(|o| o.closing.load(Ordering::Acquire))
        {
            return Err(error("CoreAudio cleanup is still owned"));
        }
        let index = owners
            .iter()
            .position(Option::is_none)
            .ok_or_else(|| error("CoreAudio owner limit reached"))?;
        let thread_entry = entry.clone();
        let handle = thread::Builder::new()
            .name("cpal-owned-coreaudio".into())
            .spawn(move || {
                if start_rx.recv().is_err() {
                    return;
                }
                let supervisor_entry = thread_entry.clone();
                let failed_ready = ready_tx.clone();
                let actual = thread::Builder::new()
                    .name("cpal-coreaudio-control".into())
                    .spawn(move || {
                        thread_entry
                            .owner_thread
                            .store(unsafe { libc::pthread_self() } as usize, Ordering::Release);
                        let mut resources = Resources::new(thread_entry.clone());
                        let outcome = catch_unwind(AssertUnwindSafe(|| {
                            let built = catch_unwind(AssertUnwindSafe(|| build(&mut resources)));
                            let mut setup = match built {
                                Ok(Ok(setup)) => {
                                    let _ = ready_tx.send(Ok(()));
                                    Some(setup)
                                }
                                Ok(Err(e)) => {
                                    thread_entry.closing.store(true, Ordering::Release);
                                    thread_entry.requests.store(1, Ordering::Release);
                                    let _ = ready_tx.send(Err(e));
                                    None
                                }
                                Err(payload) => {
                                    resources.panic = Some(payload);
                                    thread_entry.closing.store(true, Ordering::Release);
                                    thread_entry.requests.store(1, Ordering::Release);
                                    let _ = ready_tx
                                        .send(Err(error("CoreAudio initialization panicked")));
                                    None
                                }
                            };
                            let mut attempted = 0;
                            loop {
                                if thread_entry.closing.load(Ordering::Acquire) {
                                    let request = thread_entry.requests.load(Ordering::Acquire);
                                    if request > attempted {
                                        attempted = request;
                                        // A nonzero native close result has uncertain effects. The pinned API
                                        // contract does not authorize reissuing that phase. Only Pending waits
                                        // may observe this SAME owner's eventual success; native errors park it.
                                        let prior = thread_entry
                                            .completion
                                            .lock()
                                            .ok()
                                            .and_then(|state| state.failure);
                                        let closed = match prior {
                                            Some(failure) => Err(failure),
                                            None => resources.close(),
                                        };
                                        if let Ok(mut state) = thread_entry.completion.lock() {
                                            state.attempt = request;
                                            state.failure = closed.err();
                                            state.native_finished = closed.is_ok();
                                            thread_entry.changed.notify_all();
                                        }
                                        if closed.is_ok() {
                                            // No native handles/listeners remain. User captures are destroyed here,
                                            // on the retained owner, before its body/TLS join can be acknowledged.
                                            drop(setup.take());
                                            return;
                                        }
                                    }
                                }
                                match rx.recv_timeout(Duration::from_millis(10)) {
                                    Ok(Command::Play(reply)) => {
                                        let _ = reply.send(
                                            if thread_entry.closing.load(Ordering::Acquire) {
                                                Err(error("CoreAudio stream is closing"))
                                            } else {
                                                setup
                                                    .as_mut()
                                                    .ok_or_else(|| {
                                                        error("CoreAudio stream unavailable")
                                                    })
                                                    .and_then(|s| s.unit.start())
                                            },
                                        );
                                    }
                                    Ok(Command::Pause(reply)) => {
                                        let result = if thread_entry.closing.load(Ordering::Acquire)
                                        {
                                            Err(error("CoreAudio stream is closing"))
                                        } else {
                                            setup
                                                .as_mut()
                                                .ok_or_else(|| {
                                                    error("CoreAudio stream unavailable")
                                                })
                                                .and_then(|s| s.unit.stop())
                                        };
                                        let _ = reply.send(result);
                                    }
                                    Ok(Command::Buffer(reply)) => {
                                        let result = if thread_entry.closing.load(Ordering::Acquire)
                                        {
                                            Err(error("CoreAudio stream is closing"))
                                        } else {
                                            setup
                                                .as_ref()
                                                .ok_or_else(|| {
                                                    error("CoreAudio stream unavailable")
                                                })
                                                .and_then(|s| {
                                                    device::get_device_buffer_frame_size(&s.unit)
                                                        .map(|n| n as u32)
                                                        .map_err(Error::from)
                                                })
                                        };
                                        let _ = reply.send(result);
                                    }
                                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                                        // Entry itself retains the sender. Unexpected disconnect never means close.
                                        thread_entry.closing.store(true, Ordering::Release);
                                    }
                                }
                                if !thread_entry.closing.load(Ordering::Acquire) {
                                    if let Some(setup) = setup.as_mut() {
                                        let events = resources.events.swap(0, Ordering::AcqRel);
                                        let panicked = resources
                                            .tokens
                                            .borrow()
                                            .iter()
                                            .any(|registration| registration.failed());
                                        if events != 0 || panicked {
                                            let delivered = catch_unwind(AssertUnwindSafe(|| {
                                                setup.on_events(&mut resources, events, panicked)
                                            }));
                                            match delivered {
                                                Ok(Ok(())) => {}
                                                Ok(Err(_)) => {
                                                    thread_entry
                                                        .closing
                                                        .store(true, Ordering::Release);
                                                    thread_entry
                                                        .requests
                                                        .fetch_add(1, Ordering::AcqRel);
                                                }
                                                Err(payload) => {
                                                    resources.panic = Some(payload);
                                                    thread_entry
                                                        .closing
                                                        .store(true, Ordering::Release);
                                                    thread_entry
                                                        .requests
                                                        .fetch_add(1, Ordering::AcqRel);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }));
                        if let Err(payload) = outcome {
                            // Exact native handles/listeners/tokens remain in Resources. No
                            // error branch drops them, retries uncertain calls or permits replacement.
                            resources.panic = Some(payload);
                            thread_entry.request_close();
                            {
                                let mut state = thread_entry
                                    .completion
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner());
                                state.failure = Some(OwnedCoreAudioCloseError::OwnerPanicked);
                                thread_entry.changed.notify_all();
                            }
                            loop {
                                thread::park();
                            }
                        }
                    });
                match actual {
                    Ok(actual) => {
                        let joined = actual.join();
                        // Keep native owner identity valid through every actual TLS destructor.
                        supervisor_entry.owner_thread.store(0, Ordering::Release);
                        if let Err(payload) = joined {
                            supervisor_entry.request_close();
                            for token in &supervisor_entry.callback_tokens {
                                callback_gate::revoke_exact(token.load(Ordering::SeqCst));
                            }
                            *supervisor_entry
                                .join_panic
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()) = Some(payload);
                            let mut state = supervisor_entry
                                .completion
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            state.failure = Some(OwnedCoreAudioCloseError::OwnerPanicked);
                            state.joined = true;
                            supervisor_entry.changed.notify_all();
                        } else {
                            let mut state = supervisor_entry
                                .completion
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            // Actual owner body and all its TLS destructors have returned.
                            state.joined = true;
                            supervisor_entry.changed.notify_all();
                        }
                    }
                    Err(_) => {
                        // No actual thread or native handle was ever created.
                        let _ = failed_ready
                            .send(Err(error("CoreAudio control thread could not start")));
                        let mut state = supervisor_entry
                            .completion
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        state.native_finished = true;
                        state.joined = true;
                        supervisor_entry.changed.notify_all();
                    }
                }
            })
            .map_err(|_| error("CoreAudio owner thread could not start"))?;
        *entry
            .join
            .lock()
            .map_err(|_| error("CoreAudio owner handle unavailable"))? = Some(handle);
        owners[index] = Some(entry.clone());
        // No native resources exist before handle publication under the registry lock.
        start_tx
            .send(())
            .map_err(|_| error("CoreAudio owner startup failed"))?;
        drop(owners);
        let stream = Self { entry, index };
        match ready_rx.recv_timeout(DEADLINE) {
            Ok(Ok(())) => Ok(stream),
            Ok(Err(original)) => {
                let _ = stream.close_owned(DEADLINE);
                Err(original)
            }
            Err(_) => {
                stream.request_close();
                Err(error("CoreAudio owner startup did not complete"))
            }
        }
    }
    fn request_close(&self) -> usize {
        self.entry.request_close()
    }
    /// Blocking, fallible close. Call only from an already retained non-UI native worker.
    /// Timeout/error leaves the exact resources and worker in the global owned registry.
    pub fn close_owned(&self, timeout: Duration) -> Result<(), OwnedCoreAudioCloseError> {
        if reentrant() {
            return Err(OwnedCoreAudioCloseError::Reentrant);
        }
        let _serial = self
            .entry
            .join_serial
            .lock()
            .map_err(|_| OwnedCoreAudioCloseError::Ownership)?;
        let request = self.request_close();
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(OwnedCoreAudioCloseError::Ownership)?;
        let mut state = self
            .entry
            .completion
            .lock()
            .map_err(|_| OwnedCoreAudioCloseError::Ownership)?;
        while !state.joined && state.failure.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(OwnedCoreAudioCloseError::Pending);
            }
            let (next, wait) = self
                .entry
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| OwnedCoreAudioCloseError::Ownership)?;
            state = next;
            if wait.timed_out() && !state.joined && state.failure.is_none() {
                return Err(OwnedCoreAudioCloseError::Pending);
            }
        }
        if let Some(failure) = state.failure {
            return Err(failure);
        }
        if !state.joined || !state.native_finished {
            return Err(OwnedCoreAudioCloseError::Ownership);
        }
        drop(state);
        // The native body can finish before TLS destructors. Only actual join acknowledges both.
        let handle = self
            .entry
            .join
            .lock()
            .map_err(|_| OwnedCoreAudioCloseError::Ownership)?
            .take();
        if let Some(handle) = handle {
            if let Err(payload) = handle.join() {
                *self
                    .entry
                    .join_panic
                    .lock()
                    .map_err(|_| OwnedCoreAudioCloseError::Ownership)? = Some(payload);
                return Err(OwnedCoreAudioCloseError::OwnerPanicked);
            }
        } else if self
            .entry
            .join_panic
            .lock()
            .map_err(|_| OwnedCoreAudioCloseError::Ownership)?
            .is_some()
        {
            return Err(OwnedCoreAudioCloseError::OwnerPanicked);
        }
        let mut owners = registry()
            .lock()
            .map_err(|_| OwnedCoreAudioCloseError::Ownership)?;
        if owners[self.index]
            .as_ref()
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry))
        {
            owners[self.index] = None;
        }
        Ok(())
    }
    pub fn play(&self) -> Result<(), Error> {
        self.command(Command::Play)
    }
    pub fn pause(&self) -> Result<(), Error> {
        self.command(Command::Pause)
    }
    fn command(
        &self,
        make: impl FnOnce(mpsc::Sender<Result<(), Error>>) -> Command,
    ) -> Result<(), Error> {
        if reentrant() || self.entry.closing.load(Ordering::Acquire) {
            return Err(error("CoreAudio control is closing or reentrant"));
        }
        let (tx, rx) = mpsc::channel();
        self.entry
            .commands
            .try_send(make(tx))
            .map_err(|_| error("CoreAudio control queue unavailable"))?;
        rx.recv_timeout(DEADLINE)
            .map_err(|_| error("CoreAudio control remains owned"))?
    }
    pub fn buffer_size(&self) -> Result<u32, Error> {
        if reentrant() || self.entry.closing.load(Ordering::Acquire) {
            return Err(error("CoreAudio control is closing or reentrant"));
        }
        let (tx, rx) = mpsc::channel();
        self.entry
            .commands
            .try_send(Command::Buffer(tx))
            .map_err(|_| error("CoreAudio control queue unavailable"))?;
        rx.recv_timeout(DEADLINE)
            .map_err(|_| error("CoreAudio control remains owned"))?
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        self.request_close();
    }
}

/// Includes failed startup and Drop-requested owners. Never substitutes another platform's Drop.
pub(super) fn drain_owned(
    timeout: Duration,
    require_empty: bool,
) -> Result<(), OwnedCoreAudioCloseError> {
    if reentrant() {
        return Err(OwnedCoreAudioCloseError::Reentrant);
    }
    let owners: Vec<_> = registry()
        .lock()
        .map_err(|_| OwnedCoreAudioCloseError::Ownership)?
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            entry
                .as_ref()
                .filter(|entry| entry.closing.load(Ordering::Acquire))
                .map(|entry| Stream {
                    index,
                    entry: entry.clone(),
                })
        })
        .collect();
    let mut failure = None;
    for owner in owners {
        if let Err(error) = owner.close_owned(timeout) {
            // Preserve permanent native/ownership failure over merely pending observation.
            if failure.is_none() || failure == Some(OwnedCoreAudioCloseError::Pending) {
                failure = Some(error);
            }
        }
    }
    let owners = registry()
        .lock()
        .map_err(|_| OwnedCoreAudioCloseError::Ownership)?;
    let pending = owners
        .iter()
        .flatten()
        .any(|owner| owner.closing.load(Ordering::Acquire));
    if let Some(failure) = failure {
        return Err(failure);
    }
    if pending {
        return Err(OwnedCoreAudioCloseError::Pending);
    }
    if require_empty && owners.iter().any(Option::is_some) {
        return Err(OwnedCoreAudioCloseError::Ownership);
    }
    Ok(())
}

struct UnitState {
    raw: NativeUnit,
    initialized: bool,
    start_attempted: bool,
    stop_failure: Option<i32>,
}
pub(super) trait UnitProperty: Copy {}
impl UnitProperty for u32 {}
impl UnitProperty for f64 {}
impl UnitProperty for objc2_core_audio_types::AudioStreamBasicDescription {}

#[derive(Clone)]
pub(super) struct AudioUnit {
    state: Rc<RefCell<UnitState>>,
    registrations: Rc<RefCell<Vec<Registration>>>,
    owner: Arc<Entry>,
}
impl AudioUnit {
    pub fn is_closing(&self) -> bool {
        self.owner.closing.load(Ordering::SeqCst)
    }
    pub fn set_property<T>(
        &mut self,
        id: u32,
        scope: Scope,
        element: Element,
        value: Option<&T>,
    ) -> Result<(), coreaudio::Error> {
        if self.owner.closing.load(Ordering::SeqCst) {
            return Err(coreaudio::Error::Unspecified);
        }
        unsafe {
            coreaudio::audio_unit::set_property(self.state.borrow().raw, id, scope, element, value)
        }
    }
    pub fn get_property<T: UnitProperty>(
        &self,
        id: u32,
        scope: Scope,
        element: Element,
    ) -> Result<T, coreaudio::Error> {
        if self.owner.closing.load(Ordering::SeqCst) {
            return Err(coreaudio::Error::Unspecified);
        }
        // Zero initialization and exact size check avoid assuming a partial native write initialized T.
        let mut data = std::mem::MaybeUninit::<T>::zeroed();
        let mut size = std::mem::size_of::<T>() as u32;
        let code = unsafe {
            AudioUnitGetProperty(
                self.state.borrow().raw,
                id,
                scope as u32,
                element as u32,
                NonNull::from(&mut data).cast(),
                NonNull::from(&mut size),
            )
        };
        coreaudio::Error::from_os_status(code)?;
        if size as usize != std::mem::size_of::<T>() {
            return Err(coreaudio::Error::Unspecified);
        }
        Ok(unsafe { data.assume_init() })
    }
    pub fn initialize(&mut self) -> Result<(), Error> {
        if self.owner.closing.load(Ordering::SeqCst) {
            return Err(error("CoreAudio construction is closing"));
        }
        let mut state = self.state.borrow_mut();
        state.initialized = true;
        status(unsafe { AudioUnitInitialize(state.raw) })
    }
    pub fn start(&mut self) -> Result<(), Error> {
        if self.owner.closing.load(Ordering::SeqCst) {
            return Err(error("CoreAudio stream is closing"));
        }
        let mut state = self.state.borrow_mut();
        state.start_attempted = true;
        let code = unsafe { AudioOutputUnitStart(state.raw) };
        if code != 0 {
            self.owner.request_close();
        }
        status(code)
    }
    pub fn stop(&mut self) -> Result<(), Error> {
        let mut state = self.state.borrow_mut();
        if state.stop_failure.is_some() {
            return Err(error("CoreAudio stop failed and remains owned"));
        }
        if state.start_attempted {
            let code = unsafe { AudioOutputUnitStop(state.raw) };
            if code != 0 {
                state.stop_failure = Some(code);
                self.owner.request_close();
                return status(code);
            }
            state.start_attempted = false;
        }
        Ok(())
    }
    pub fn set_render_callback<F>(&mut self, format: SampleFormat, callback: F) -> Result<(), Error>
    where
        F: FnMut(RawArgs) -> Result<(), ()> + Send + 'static,
    {
        self.register(Body::Render(Box::new(callback)), format, false)
    }
    pub fn set_input_callback<F>(
        &mut self,
        config: StreamConfig,
        format: SampleFormat,
        callback: F,
    ) -> Result<(), Error>
    where
        F: FnMut(RawArgs) -> Result<(), ()> + Send + 'static,
    {
        let maximum: u32 = self.get_property(
            kAudioUnitProperty_MaximumFramesPerSlice,
            Scope::Global,
            Element::Output,
        )?;
        let bytes_per_frame = (config.channels as usize)
            .checked_mul(format.sample_size())
            .ok_or_else(|| error("CoreAudio input size overflow"))?;
        let body = callback_gate::input_body(
            self.state.borrow().raw,
            config.channels as u32,
            bytes_per_frame,
            maximum as usize,
            Box::new(callback),
        )?;
        self.register(body, format, true)
    }
    fn register(&mut self, body: Body, format: SampleFormat, input: bool) -> Result<(), Error> {
        if self.registrations.borrow().len() >= 16 {
            return Err(error("CoreAudio callback bound reached"));
        }
        let token = Token::install(body, format)?;
        let registration = Registration {
            token,
            retired: None,
            released: false,
        };
        let data = AURenderCallbackStruct {
            inputProc: Some(callback_gate::render),
            inputProcRefCon: registration.token.refcon(),
        };
        let index = self.registrations.borrow().len();
        if index >= 16 {
            registration.token.revoke();
            return Err(error("CoreAudio callback bound reached"));
        }
        self.owner.callback_tokens[index].store(registration.token.0, Ordering::SeqCst);
        self.registrations.borrow_mut().push(registration);
        if self.owner.closing.load(Ordering::SeqCst) {
            callback_gate::revoke_exact(self.owner.callback_tokens[index].load(Ordering::SeqCst));
            return Err(error("CoreAudio construction is closing"));
        }
        let (id, scope, element) = if input {
            (
                kAudioOutputUnitProperty_SetInputCallback,
                Scope::Global,
                Element::Output,
            )
        } else {
            (
                kAudioUnitProperty_SetRenderCallback,
                Scope::Input,
                Element::Output,
            )
        };
        self.set_property(id, scope, element, Some(&data))?;
        Ok(())
    }
}

struct Registration {
    token: Token,
    retired: Option<Retired>,
    released: bool,
}
impl Registration {
    fn revoke(&mut self) -> Result<(), Error> {
        if self.released {
            return Ok(());
        }
        self.token.revoke();
        if self.retired.is_none() {
            self.retired = Some(self.token.retire()?);
        }
        Ok(())
    }
    fn failed(&self) -> bool {
        !self.released && self.token.failed()
    }
    fn release(&mut self) -> Result<(), Error> {
        if !self.released {
            self.token.release()?;
            self.released = true;
            drop(self.retired.take());
        }
        Ok(())
    }
}
struct Listener {
    object: AudioObjectID,
    address: AudioObjectPropertyAddress,
    registration: usize,
    remove_needed: bool,
    remove_failure: Option<i32>,
}
pub(super) struct Resources {
    owner: Arc<Entry>,
    units: Vec<Rc<RefCell<UnitState>>>,
    listeners: Vec<Listener>,
    tokens: Rc<RefCell<Vec<Registration>>>,
    loopbacks: Vec<LoopbackDevice>,
    events: Arc<AtomicU32>,
    panic: Option<Box<dyn Any + Send>>,
}
impl Resources {
    fn new(owner: Arc<Entry>) -> Self {
        Self {
            owner,
            units: Vec::with_capacity(2),
            listeners: Vec::with_capacity(8),
            tokens: Rc::new(RefCell::new(Vec::with_capacity(16))),
            loopbacks: Vec::with_capacity(1),
            events: Arc::new(AtomicU32::new(0)),
            panic: None,
        }
    }
    pub fn is_closing(&self) -> bool {
        self.owner.closing.load(Ordering::SeqCst)
    }
    pub fn unit(&mut self, default: bool) -> Result<AudioUnit, Error> {
        if self.is_closing() {
            return Err(error("CoreAudio construction is closing"));
        }
        if self.units.len() >= 2 {
            return Err(error("CoreAudio unit bound reached"));
        }
        let desc = AudioComponentDescription {
            componentType: kAudioUnitType_Output,
            componentSubType: if default {
                kAudioUnitSubType_DefaultOutput
            } else {
                kAudioUnitSubType_HALOutput
            },
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let component =
            unsafe { AudioComponentFindNext(std::ptr::null_mut(), NonNull::from(&desc)) };
        if component.is_null() {
            return Err(error("CoreAudio component unavailable"));
        }
        if self.is_closing() {
            return Err(error("CoreAudio construction is closing"));
        }
        let mut raw: NativeUnit = std::ptr::null_mut();
        let result = unsafe { AudioComponentInstanceNew(component, NonNull::from(&mut raw)) };
        // Own any returned instance BEFORE inspecting the status, including partial initialization.
        if raw.is_null() {
            status(result)?;
            return Err(error("CoreAudio returned no unit"));
        }
        let state = Rc::new(RefCell::new(UnitState {
            raw,
            initialized: false,
            start_attempted: false,
            stop_failure: None,
        }));
        self.units.push(state.clone());
        status(result)?;
        if self.is_closing() {
            return Err(error("CoreAudio construction is closing"));
        }
        Ok(AudioUnit {
            state,
            registrations: self.tokens.clone(),
            owner: self.owner.clone(),
        })
    }
    pub fn loopback(&mut self, device: &Device) -> Result<Device, Error> {
        if self.is_closing() {
            return Err(error("CoreAudio construction is closing"));
        }
        if !self.loopbacks.is_empty() {
            return Err(error("CoreAudio loopback bound reached"));
        }
        self.loopbacks.push(LoopbackDevice::empty());
        let owner = self.owner.clone();
        let owned = &mut self.loopbacks[0];
        owned.create(device, || !owner.closing.load(Ordering::SeqCst))?;
        Ok(owned.aggregate_device.clone())
    }
    pub fn listener(
        &mut self,
        object: AudioObjectID,
        selector: u32,
        mask: u32,
    ) -> Result<usize, Error> {
        let reusable = self
            .listeners
            .iter()
            .position(|listener| !listener.remove_needed);
        if reusable.is_none() && self.listeners.len() >= 8 {
            return Err(error("CoreAudio listener bound reached"));
        }
        let address = AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let token = Token::install(
            Body::Property {
                events: self.events.clone(),
                mask,
            },
            SampleFormat::F32,
        )?;
        let refcon = token.refcon();
        let registration = Registration {
            token,
            retired: None,
            released: false,
        };
        let (index, registration_index) = if let Some(index) = reusable {
            let registration_index = self.listeners[index].registration;
            // Previous registration was removed, drained and released before reuse.
            self.tokens.borrow_mut()[registration_index] = registration;
            (index, registration_index)
        } else {
            let registration_index = self.tokens.borrow().len();
            self.tokens.borrow_mut().push(registration);
            self.listeners.push(Listener {
                object,
                address,
                registration: registration_index,
                remove_needed: false,
                remove_failure: None,
            });
            (self.listeners.len() - 1, registration_index)
        };
        // A failed Add can have partially registered. Retain the exact remove tuple first.
        self.listeners[index] = Listener {
            object,
            address,
            registration: registration_index,
            remove_needed: true,
            remove_failure: None,
        };
        self.owner.callback_tokens[registration_index].store(refcon.addr(), Ordering::SeqCst);
        if self.is_closing() {
            callback_gate::revoke_exact(refcon.addr());
            self.listeners[index].remove_needed = false; // native Add was never entered
            return Err(error("CoreAudio monitor is closing"));
        }
        status(unsafe {
            AudioObjectAddPropertyListener(
                object,
                NonNull::from(&address),
                Some(callback_gate::property),
                refcon,
            )
        })?;
        Ok(index)
    }
    pub fn remove_listener(&mut self, index: usize) -> Result<(), Error> {
        let listener = &mut self.listeners[index];
        let mut registrations = self.tokens.borrow_mut();
        let registration = &mut registrations[listener.registration];
        registration.revoke()?;
        if listener.remove_failure.is_some() {
            return Err(error("CoreAudio listener removal failed and remains owned"));
        }
        if listener.remove_needed {
            let code = unsafe {
                AudioObjectRemovePropertyListener(
                    listener.object,
                    NonNull::from(&listener.address),
                    Some(callback_gate::property),
                    registration.token.refcon(),
                )
            };
            if code != 0 {
                listener.remove_failure = Some(code);
                self.owner.request_close();
                return status(code);
            }
            listener.remove_needed = false;
        }
        registration.release()?;
        Ok(())
    }
    pub fn rate_changed(&self) -> bool {
        self.events.fetch_and(!16, Ordering::AcqRel) & 16 != 0
    }
    pub fn monitor(&mut self, device: AudioObjectID, default: bool) -> Result<usize, Error> {
        if default {
            self.listener(
                kAudioObjectSystemObject as _,
                kAudioHardwarePropertyDefaultOutputDevice,
                DEFAULT,
            )?;
        } else {
            self.listener(device, kAudioDevicePropertyDeviceIsAlive, ALIVE)?;
            self.listener(device, kAudioDevicePropertyNominalSampleRate, RATE)?;
        }
        self.listener(device, kAudioDeviceProcessorOverload, OVERLOAD)
    }
    fn close(&mut self) -> Result<(), CloseFailure> {
        for registration in self.tokens.borrow().iter() {
            if !registration.released {
                registration.token.revoke();
            }
        }
        for registration in self.tokens.borrow_mut().iter_mut() {
            registration
                .revoke()
                .map_err(|_| OwnedCoreAudioCloseError::Ownership)?;
        }
        // Exact native remove tuple retained even for a partially failed Add.
        for listener in &mut self.listeners {
            let mut registrations = self.tokens.borrow_mut();
            let registration = &mut registrations[listener.registration];
            if let Some(code) = listener.remove_failure {
                return Err(OwnedCoreAudioCloseError::Native {
                    phase: CoreAudioClosePhase::ListenerRemove,
                    status: code,
                });
            }
            if listener.remove_needed {
                let code = unsafe {
                    AudioObjectRemovePropertyListener(
                        listener.object,
                        NonNull::from(&listener.address),
                        Some(callback_gate::property),
                        registration.token.refcon(),
                    )
                };
                if code != 0 {
                    listener.remove_failure = Some(code);
                    return Err(OwnedCoreAudioCloseError::Native {
                        phase: CoreAudioClosePhase::ListenerRemove,
                        status: code,
                    });
                }
                listener.remove_needed = false;
            }
            registration
                .release()
                .map_err(|_| OwnedCoreAudioCloseError::Ownership)?;
        }
        for unit in &self.units {
            let mut unit = unit.borrow_mut();
            if let Some(code) = unit.stop_failure {
                return Err(OwnedCoreAudioCloseError::Native {
                    phase: CoreAudioClosePhase::Stop,
                    status: code,
                });
            }
            if unit.start_attempted {
                let code = unsafe { AudioOutputUnitStop(unit.raw) };
                if code != 0 {
                    unit.stop_failure = Some(code);
                    return Err(OwnedCoreAudioCloseError::Native {
                        phase: CoreAudioClosePhase::Stop,
                        status: code,
                    });
                }
                unit.start_attempted = false;
            }
            if unit.initialized {
                let code = unsafe { AudioUnitUninitialize(unit.raw) };
                if code != 0 {
                    return Err(OwnedCoreAudioCloseError::Native {
                        phase: CoreAudioClosePhase::Uninitialize,
                        status: code,
                    });
                }
                unit.initialized = false;
            }
            if !unit.raw.is_null() {
                let code = unsafe { AudioComponentInstanceDispose(unit.raw) };
                if code != 0 {
                    return Err(OwnedCoreAudioCloseError::Native {
                        phase: CoreAudioClosePhase::Dispose,
                        status: code,
                    });
                }
                unit.raw = std::ptr::null_mut();
            }
        }
        for loopback in &mut self.loopbacks {
            loopback.close_owned()?;
        }
        // Only after successful native completion are body/buffers/panic payload freed OFF RT.
        for registration in self.tokens.borrow_mut().iter_mut() {
            registration
                .release()
                .map_err(|_| OwnedCoreAudioCloseError::Ownership)?;
        }
        self.tokens.borrow_mut().clear();
        self.panic.take();
        Ok(())
    }
}

pub(super) struct Setup {
    pub unit: AudioUnit,
    pub device: AudioObjectID,
    pub overload_listener: usize,
    pub latency: Option<Arc<AtomicUsize>>,
    pub error_callback: crate::host::ErrorCallbackArc,
}
impl Setup {
    fn on_events(
        &mut self,
        resources: &mut Resources,
        events: u32,
        panicked: bool,
    ) -> Result<(), Error> {
        if panicked {
            crate::host::emit_error(
                &self.error_callback,
                error("CoreAudio data callback failed"),
            );
            return Err(error("CoreAudio data callback failed"));
        }
        if resources.is_closing() {
            return Ok(());
        }
        if events & (ALIVE | RATE) != 0 {
            self.unit.stop()?;
            crate::host::emit_error(
                &self.error_callback,
                error("CoreAudio device disconnected or changed rate"),
            );
        }
        if resources.is_closing() {
            return Ok(());
        }
        if events & OVERLOAD != 0 {
            crate::host::emit_error(&self.error_callback, ErrorKind::Xrun.into());
        }
        if resources.is_closing() {
            return Ok(());
        }
        if events & DEFAULT != 0 {
            // No reroute registration or native access occurs after owner close admission.
            resources.remove_listener(self.overload_listener)?;
            if resources.is_closing() {
                return Ok(());
            }
            match super::default_output_device() {
                None => {
                    self.unit.stop()?;
                    crate::host::emit_error(
                        &self.error_callback,
                        ErrorKind::DeviceNotAvailable.into(),
                    );
                }
                Some(device) => {
                    if resources.is_closing() {
                        return Ok(());
                    }
                    self.device = device.audio_device_id;
                    self.overload_listener =
                        resources.listener(self.device, kAudioDeviceProcessorOverload, OVERLOAD)?;
                    if resources.is_closing() {
                        return Ok(());
                    }
                    if let Some(latency) = &self.latency {
                        let frames = device::get_device_buffer_frame_size(&self.unit).unwrap_or(0)
                            + device::get_device_extra_latency_frames(&self.unit, Scope::Output);
                        latency.store(frames, Ordering::Relaxed);
                    }
                    crate::host::emit_error(&self.error_callback, ErrorKind::DeviceChanged.into());
                }
            }
        }
        Ok(())
    }
}

/// Prewarmed native timebase; conversion on a foreign RT callback is pure bounded arithmetic.
pub(super) struct CallbackClock {
    numer: u32,
    denom: u32,
}
impl CallbackClock {
    pub fn new() -> Result<Self, Error> {
        let mut info: mach2::mach_time::mach_timebase_info = Default::default();
        status(unsafe { mach2::mach_time::mach_timebase_info(&mut info) })?;
        if info.numer == 0 || info.denom == 0 {
            return Err(error("CoreAudio callback clock unavailable"));
        }
        Ok(Self {
            numer: info.numer,
            denom: info.denom,
        })
    }
    pub fn instant(&self, ticks: u64) -> Option<crate::StreamInstant> {
        let nanos = ticks as u128 * self.numer as u128 / self.denom as u128;
        let seconds = u64::try_from(nanos / 1_000_000_000).ok()?;
        Some(crate::StreamInstant::new(
            seconds,
            (nanos % 1_000_000_000) as u32,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static SERIAL: Mutex<()> = Mutex::new(());
    fn entry() -> Arc<Entry> {
        let (tx, _rx) = mpsc::sync_channel(1);
        Arc::new(Entry {
            owner_thread: AtomicUsize::new(0),
            closing: AtomicBool::new(false),
            callback_tokens: [const { AtomicUsize::new(0) }; 16],
            requests: AtomicUsize::new(0),
            commands: tx,
            completion: Mutex::new(Completion::default()),
            changed: Condvar::new(),
            join: Mutex::new(None),
            join_panic: Mutex::new(None),
            join_serial: Mutex::new(()),
        })
    }
    #[test]
    fn close_request_revokes_published_callback_and_blocks_later_native_construction() {
        let _serial = SERIAL.lock().unwrap();
        let entry = entry();
        let token = Token::install(
            Body::Property {
                events: Arc::new(AtomicU32::new(0)),
                mask: 1,
            },
            SampleFormat::I32,
        )
        .unwrap();
        entry.callback_tokens[0].store(token.0, Ordering::SeqCst);
        entry.request_close(); // actual atomics-only production request, no wake queue
        let mut resources = Resources::new(entry.clone());
        let unit_refused = resources.unit(false).is_err();
        let loopback_refused = resources.loopback(&Device::new(0)).is_err();
        let listener_refused = resources
            .listener(0, kAudioDeviceProcessorOverload, OVERLOAD)
            .is_err();
        let retired = token.retire().unwrap();
        token.release().unwrap();
        drop(retired);
        let cleaned = resources.close(); // no native admission occurred, so no native calls to undo
        assert!(entry.closing.load(Ordering::SeqCst));
        assert!(unit_refused && loopback_refused && listener_refused);
        assert_eq!(cleaned, Ok(()));
    }
    struct TlsHold(Arc<(Mutex<bool>, Condvar)>, mpsc::Sender<()>);
    impl Drop for TlsHold {
        fn drop(&mut self) {
            let _ = self.1.send(());
            let (flag, changed) = &*self.0;
            let guard = flag.lock().unwrap();
            let _ = changed
                .wait_timeout_while(guard, Duration::from_secs(5), |released| !*released)
                .unwrap();
        }
    }
    thread_local! { static TLS_HOLD: RefCell<Option<TlsHold>> = const { RefCell::new(None) }; }
    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            let (flag, changed) = &*self.0;
            *flag.lock().unwrap() = true;
            changed.notify_all();
        }
    }
    #[test]
    fn pending_real_owner_tls_keeps_actual_join_and_replacement_fence() {
        let _serial = SERIAL.lock().unwrap();
        let released = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Release(released.clone());
        let (entered_tx, entered_rx) = mpsc::channel();
        let opened = Stream::build(move |resources| {
            TLS_HOLD.with(|hold| *hold.borrow_mut() = Some(TlsHold(released, entered_tx)));
            // No native unit exists in this fixture. Only the actual owner/body/TLS
            // join and replacement fence are under test, never device disposal.
            Ok(Setup {
                unit: AudioUnit {
                    state: Rc::new(RefCell::new(UnitState {
                        raw: std::ptr::null_mut(),
                        initialized: false,
                        start_attempted: false,
                        stop_failure: None,
                    })),
                    registrations: resources.tokens.clone(),
                    owner: resources.owner.clone(),
                },
                device: 0,
                overload_listener: 0,
                latency: None,
                error_callback: Arc::new(Mutex::new(|_: Error| {})),
            })
        });
        if let Ok(stream) = &opened {
            stream.request_close();
        }
        let tls_entered = entered_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        let pending = drain_owned(Duration::from_millis(10), false);
        let attempted = Arc::new(AtomicBool::new(false));
        let did_attempt = attempted.clone();
        let replacement = Stream::build(move |_| {
            did_attempt.store(true, Ordering::Release);
            Err(error("must not enter"))
        });
        drop(release); // always release the retained actual owner before assertions
        let joined = drain_owned(Duration::from_secs(2), false);
        assert!(opened.is_ok() && tls_entered);
        assert_eq!(pending, Err(OwnedCoreAudioCloseError::Pending));
        assert!(replacement.is_err() && !attempted.load(Ordering::Acquire));
        assert_eq!(joined, Ok(()));
    }
}
