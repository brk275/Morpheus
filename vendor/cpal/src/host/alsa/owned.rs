//! Linux-only retained ALSA native owners. Drop revokes; only checked close and
//! actual callback/control worker joins can produce a successful release.
use super::{AlsaContext, Device, StreamInner, TriggerReceiver, TriggerSender};
use crate::host::latch::Latch;
use crate::{Error, ErrorKind, SampleFormat, StreamConfig};
use std::{
    any::Any,
    mem::ManuallyDrop,
    ops::Deref,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU8, Ordering},
        mpsc, Arc, Condvar, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const LIMIT: usize = 16;
const WAIT: Duration = Duration::from_secs(5);
const CALLBACK_ACTIVE: u8 = 1;
const CALLBACK_CLOSED: u8 = 2;
static OWNER_IDS: [AtomicI32; LIMIT] = [const { AtomicI32::new(0) }; LIMIT];
static CALLBACK_IDS: [AtomicI32; LIMIT] = [const { AtomicI32::new(0) }; LIMIT];

fn current_tid() -> i32 {
    // Linux-native thread identity, used only to reject self-waits. Actual
    // ownership/completion always comes from retained JoinHandle::join.
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}
fn reentrant() -> bool {
    let current = current_tid();
    OWNER_IDS
        .iter()
        .chain(CALLBACK_IDS.iter())
        .any(|id| id.load(Ordering::Acquire) == current)
}
pub(super) fn error(message: &'static str) -> Error {
    Error::with_message(ErrorKind::StreamInvalidated, message)
}

// Preserve the preexisting selected-direction open error contract while the
// separate resource holder records the actual native failed-open phase.
fn open_error(native: alsa::Error, direction: alsa::Direction) -> Error {
    let original = Error::from(native);
    if original.kind() == ErrorKind::UnsupportedConfig {
        Error::with_message(
            ErrorKind::UnsupportedOperation,
            match direction {
                alsa::Direction::Capture => "Device does not support input",
                alsa::Direction::Playback => "Device does not support output",
            },
        )
    } else {
        original
    }
}

/// Fixed native categories; no endpoint IDs, user strings or panic payloads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlsaCloseError {
    Pending,
    Reentrant,
    Ownership,
    OwnerPanicked,
    CallbackPanicked,
    Active,
    Open {
        status: i32,
    },
    Transport {
        status: i32,
    },
    /// A negative status in the reviewed ABI consumes/frees the primary PCM.
    /// It does NOT prove plugin/hardware retirement; never retry. A positive
    /// out-of-contract status leaves consumption unproved (the bool is false).
    Native {
        status: i32,
        primary_handle_consumed: bool,
    },
}
impl std::fmt::Display for AlsaCloseError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Pending => "ALSA native owner has not finished",
            Self::Reentrant => "ALSA self-close observation refused",
            Self::Ownership => "ALSA native ownership could not be confirmed",
            Self::OwnerPanicked => "ALSA owner or worker TLS panicked",
            Self::CallbackPanicked => "ALSA callback worker panicked",
            Self::Active => "ALSA stream remains active",
            Self::Transport { .. } => "ALSA wake descriptor cleanup failed",
            Self::Open { .. } => "ALSA open completion could not be confirmed",
            Self::Native { .. } => "ALSA native close failed and will not be retried",
        })
    }
}
impl std::error::Error for AlsaCloseError {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlsaCloseObservation {
    Pending,
    Released,
    Retained(AlsaCloseError),
}

/// Operation phase, separate from an acquired PCM's checked close result.
/// A returned negative open status transfers no PCM to this application. It is
/// not a synthetic close(0); the actual worker and its TLS still have to join.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpenPhase {
    NotEntered,
    Entered,
    Rejected(i32),
    Acquired,
}

/// The actual PCM allocation is never released by a Rust Drop fallback. It is
/// kept in Resources published in the process registry, and consumed exactly
/// once only after every callback user has really joined.
#[derive(Debug)]
pub(super) struct OwnedPcm(ManuallyDrop<alsa::PCM>);
impl OwnedPcm {
    fn new(pcm: alsa::PCM) -> Self {
        Self(ManuallyDrop::new(pcm))
    }
    fn close(self) -> Result<(), i32> {
        ManuallyDrop::into_inner(self.0).close()
    }
}
impl Deref for OwnedPcm {
    type Target = alsa::PCM;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
// Same thread-safe ALSA requirement as CPAL's preexisting StreamInner. No PCM
// reference can survive the actual callback join and Arc uniqueness check.
unsafe impl Sync for OwnedPcm {}

pub(super) enum Command {
    Play(mpsc::Sender<Result<(), Error>>),
    Pause(mpsc::Sender<Result<(), Error>>),
    Now(mpsc::Sender<crate::StreamInstant>),
}
struct Entry {
    index: usize,
    closing: Arc<AtomicBool>,
    admission: AtomicU8,
    commands: mpsc::SyncSender<Command>,
    resources: Mutex<Resources>,
    result: Mutex<Option<AlsaCloseObservation>>,
    changed: Condvar,
    supervisor: Mutex<Option<JoinHandle<()>>>,
    reap: Mutex<()>,
    panic: Mutex<Option<Box<dyn Any + Send>>>,
    period: AtomicU32,
    created: Mutex<Instant>,
}
impl Entry {
    fn request(&self) {
        // Atomics only: Stream Drop can run inside a user callback. No channel
        // wake/registry mutex/native call/join is permitted on this path.
        self.admission.fetch_or(CALLBACK_CLOSED, Ordering::SeqCst);
        self.closing.store(true, Ordering::SeqCst);
    }
    fn enter_callback(&self) -> Option<CallbackGuard<'_>> {
        self.admission
            .compare_exchange(0, CALLBACK_ACTIVE, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| CallbackGuard {
                entry: self,
                returned: false,
            })
    }
}
struct CallbackGuard<'a> {
    entry: &'a Entry,
    returned: bool,
}
impl Drop for CallbackGuard<'_> {
    fn drop(&mut self) {
        if !self.returned {
            self.entry.request();
        }
        self.entry
            .admission
            .fetch_and(!CALLBACK_ACTIVE, Ordering::SeqCst);
    }
}
static REGISTRY: OnceLock<Mutex<Vec<Option<Arc<Entry>>>>> = OnceLock::new();
fn registry() -> &'static Mutex<Vec<Option<Arc<Entry>>>> {
    REGISTRY.get_or_init(|| Mutex::new((0..LIMIT).map(|_| None).collect()))
}

#[derive(Clone)]
pub struct AlsaCloseTicket(Arc<Entry>);
impl std::fmt::Debug for AlsaCloseTicket {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("AlsaCloseTicket")
    }
}
impl AlsaCloseTicket {
    pub fn request(&self) {
        self.0.request();
    }
    /// Off UI/Tokio and off every ALSA native owner/callback only. Actual native
    /// API waits and user/TLS destructors stay on retained workers. The final
    /// fixed supervisor join epilogue has no hard OS scheduling guarantee.
    pub fn observe(&self) -> AlsaCloseObservation {
        observe(&self.0)
    }
    pub fn wait(&self, bound: Duration) -> AlsaCloseObservation {
        if reentrant() {
            return AlsaCloseObservation::Retained(AlsaCloseError::Reentrant);
        }
        let until = Instant::now().checked_add(bound);
        loop {
            let result = self.observe();
            if result != AlsaCloseObservation::Pending {
                return result;
            }
            let Some(until) = until else {
                return AlsaCloseObservation::Pending;
            };
            if Instant::now() >= until {
                return AlsaCloseObservation::Pending;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}
fn observe(entry: &Arc<Entry>) -> AlsaCloseObservation {
    if reentrant() {
        return AlsaCloseObservation::Retained(AlsaCloseError::Reentrant);
    }
    let Ok(_serial) = entry.reap.try_lock() else {
        return AlsaCloseObservation::Pending;
    };
    let Ok(mut retained) = entry.supervisor.lock() else {
        return AlsaCloseObservation::Retained(AlsaCloseError::Ownership);
    };
    let result = entry.result.lock().ok().and_then(|result| *result);
    let Some(handle) = retained.as_ref() else {
        return result.unwrap_or(AlsaCloseObservation::Retained(AlsaCloseError::Ownership));
    };
    if !handle.is_finished() {
        return AlsaCloseObservation::Pending;
    }
    let joined = retained.take().expect("retained ALSA supervisor").join();
    if let Err(payload) = joined {
        entry.request();
        *entry.panic.lock().unwrap_or_else(|e| e.into_inner()) = Some(payload);
        *entry.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(
            AlsaCloseObservation::Retained(AlsaCloseError::OwnerPanicked),
        );
    }
    let result = entry
        .result
        .lock()
        .ok()
        .and_then(|result| *result)
        .unwrap_or(AlsaCloseObservation::Retained(AlsaCloseError::Ownership));
    if result == AlsaCloseObservation::Released {
        if let Ok(mut owners) = registry().lock() {
            if owners[entry.index]
                .as_ref()
                .is_some_and(|o| Arc::ptr_eq(o, entry))
            {
                owners[entry.index] = None;
            } else {
                return AlsaCloseObservation::Retained(AlsaCloseError::Ownership);
            }
        } else {
            return AlsaCloseObservation::Retained(AlsaCloseError::Ownership);
        }
    }
    result
}

pub fn observe_retiring_alsa() -> AlsaCloseObservation {
    observe_all(false)
}
pub fn observe_alsa_shutdown() -> AlsaCloseObservation {
    observe_all(true)
}
fn observe_all(require_empty: bool) -> AlsaCloseObservation {
    if reentrant() {
        return AlsaCloseObservation::Retained(AlsaCloseError::Reentrant);
    }
    let owners = match registry().lock() {
        Ok(owners) => owners.iter().flatten().cloned().collect::<Vec<_>>(),
        Err(_) => return AlsaCloseObservation::Retained(AlsaCloseError::Ownership),
    };
    let mut pending = false;
    for owner in owners {
        if !owner.closing.load(Ordering::Acquire) {
            if require_empty {
                return AlsaCloseObservation::Retained(AlsaCloseError::Active);
            }
            continue;
        }
        match observe(&owner) {
            AlsaCloseObservation::Pending => pending = true,
            AlsaCloseObservation::Released => {}
            failed => return failed,
        }
    }
    if pending {
        AlsaCloseObservation::Pending
    } else {
        AlsaCloseObservation::Released
    }
}

type Work = Box<
    dyn FnOnce(
            &mut Resources,
            &Arc<Entry>,
            mpsc::Receiver<Command>,
            mpsc::SyncSender<Result<(), Error>>,
        ) -> AlsaCloseObservation
        + Send,
>;
fn start(work: Work) -> Result<(Arc<Entry>, mpsc::Receiver<Result<(), Error>>), Error> {
    if reentrant() {
        return Err(error("ALSA reentrant construction refused"));
    }
    let mut owners = registry()
        .lock()
        .map_err(|_| error("ALSA owner registry unavailable"))?;
    if owners
        .iter()
        .flatten()
        .any(|o| o.closing.load(Ordering::Acquire))
    {
        return Err(error("ALSA cleanup remains owned"));
    }
    let index = owners
        .iter()
        .position(Option::is_none)
        .ok_or_else(|| error("ALSA owner limit reached"))?;
    let (tx, rx) = mpsc::sync_channel(16);
    let closing = Arc::new(AtomicBool::new(false));
    let entry = Arc::new(Entry {
        index,
        closing: closing.clone(),
        admission: AtomicU8::new(0),
        commands: tx,
        resources: Mutex::new(Resources::new(closing)),
        result: Mutex::new(None),
        changed: Condvar::new(),
        supervisor: Mutex::new(None),
        reap: Mutex::new(()),
        panic: Mutex::new(None),
        period: AtomicU32::new(0),
        created: Mutex::new(Instant::now()),
    });
    let (start, admitted) = mpsc::sync_channel(1);
    let (ready, answer) = mpsc::sync_channel(1);
    let child = entry.clone();
    let handle = thread::Builder::new()
        .name("cpal-alsa-owner".into())
        .spawn(move || {
            if admitted.recv().is_err() {
                return;
            }
            let actual_entry = child.clone();
            let failed_ready = ready.clone();
            let (native_start, native_admitted) = mpsc::sync_channel(1);
            let actual = thread::Builder::new()
                .name("cpal-alsa-control".into())
                .spawn(move || {
                    // Also reject self-waits from capture/TLS destructors on
                    // a failed startup gate, before any PCM has been admitted.
                    OWNER_IDS[index].store(current_tid(), Ordering::Release);
                    if native_admitted.recv().is_err() {
                        actual_entry.request();
                        return AlsaCloseObservation::Released;
                    }
                    let Ok(mut resources) = actual_entry.resources.lock() else {
                        let _ = ready.send(Err(error("ALSA resources unavailable")));
                        return AlsaCloseObservation::Retained(AlsaCloseError::Ownership);
                    };
                    match catch_unwind(AssertUnwindSafe(|| {
                        work(&mut resources, &actual_entry, rx, ready)
                    })) {
                        Ok(result) => result,
                        Err(payload) => {
                            actual_entry.request();
                            *actual_entry.panic.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(payload);
                            AlsaCloseObservation::Retained(AlsaCloseError::OwnerPanicked)
                        }
                    }
                });
            let result = match actual {
                Ok(actual) => {
                    // The actual handle is now retained in this already owned
                    // supervisor frame. Only now may the control worker enter C.
                    if native_start.send(()).is_err() {
                        child.request();
                    }
                    match actual.join() {
                        Ok(result) => result,
                        Err(payload) => {
                            child.request();
                            *child.panic.lock().unwrap_or_else(|e| e.into_inner()) = Some(payload);
                            AlsaCloseObservation::Retained(AlsaCloseError::OwnerPanicked)
                        }
                    }
                }
                Err(_) => {
                    child.request();
                    let _ = failed_ready.send(Err(error("Could not start ALSA native owner")));
                    AlsaCloseObservation::Released
                }
            };
            if result != AlsaCloseObservation::Released {
                child.request();
            }
            OWNER_IDS[index].store(0, Ordering::Release);
            *child.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
            child.changed.notify_all();
        })
        .map_err(|_| error("Could not start ALSA ownership supervisor"))?;
    *entry.supervisor.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    owners[index] = Some(entry.clone());
    if start.send(()).is_err() {
        entry.request();
        return Err(error("ALSA supervisor admission failed"));
    }
    Ok((entry, answer))
}

/// A published resource holder, not a public stream or stack-only cleanup guard.
pub(super) struct Resources {
    pub(super) closing: Arc<AtomicBool>,
    pcm: Option<Arc<OwnedPcm>>,
    inner: Option<Arc<StreamInner>>,
    context: Option<Arc<AlsaContext>>,
    callback: Option<JoinHandle<()>>,
    trigger: Option<TriggerSender>,
    receiver: Option<Arc<TriggerReceiver>>,
    latch: Option<Latch>,
    native_return: Option<Result<(), i32>>,
    open_phase: OpenPhase,
    transport_return: Option<Result<(), i32>>,
    sender_return: Option<Result<(), i32>>,
    callback_panic: Option<Box<dyn Any + Send>>,
}
impl Resources {
    fn new(closing: Arc<AtomicBool>) -> Self {
        Self {
            closing,
            pcm: None,
            inner: None,
            context: None,
            callback: None,
            trigger: None,
            receiver: None,
            latch: None,
            native_return: None,
            open_phase: OpenPhase::NotEntered,
            transport_return: None,
            sender_return: None,
            callback_panic: None,
        }
    }
    pub fn active(&self) -> Result<(), Error> {
        if self.closing.load(Ordering::SeqCst) {
            Err(error("ALSA construction is closing"))
        } else {
            Ok(())
        }
    }
    pub fn closing(&self) -> Arc<AtomicBool> {
        self.closing.clone()
    }
    pub fn open(&mut self, id: &str, direction: alsa::Direction) -> Result<Arc<OwnedPcm>, Error> {
        self.active()?;
        if self.pcm.is_some() || self.open_phase != OpenPhase::NotEntered {
            return Err(error("ALSA PCM open already entered"));
        }
        // CString validation happens before native entry. A returned ALSA open
        // rejection and an acquired PCM are distinct phases; only the latter
        // transfers a handle that this owner must close.
        let name = std::ffi::CString::new(id).map_err(|_| ErrorKind::InvalidInput)?;
        let _serial = super::ALSA_OPEN_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.active()?;
        self.open_phase = OpenPhase::Entered;
        let pcm = match alsa::PCM::open(&name, direction, true) {
            Ok(pcm) => Arc::new(OwnedPcm::new(pcm)),
            Err(native) => {
                self.open_phase = OpenPhase::Rejected(-native.errno());
                return Err(open_error(native, direction));
            }
        };
        self.pcm = Some(pcm.clone());
        self.open_phase = OpenPhase::Acquired;
        self.active()?;
        Ok(pcm)
    }
    fn close(&mut self, entry: &Entry) -> AlsaCloseObservation {
        entry.request();
        if let Some(latch) = self.latch.as_ref() {
            latch.release();
        }
        if self.callback.is_some() {
            if let Some(trigger) = self.trigger.as_ref() {
                trigger.wakeup();
            }
            if let Err(payload) = self.callback.take().expect("retained ALSA callback").join() {
                self.callback_panic = Some(payload);
            }
            CALLBACK_IDS[entry.index].store(0, Ordering::Release);
        }
        // Context and the original PCM remain retained until all temporary
        // parameters, callback captures, transfer buffers and callback TLS ended.
        self.inner.take();
        match self.open_phase {
            OpenPhase::Entered => {
                return AlsaCloseObservation::Retained(AlsaCloseError::Open { status: 0 });
            }
            OpenPhase::Rejected(status) if status >= 0 => {
                return AlsaCloseObservation::Retained(AlsaCloseError::Open { status });
            }
            // The documented negative return rejected the operation without
            // transferring a PCM. Do not call close on an invalid pointer or
            // invent native_return=Ok. The real control/TLS/supervisor joins
            // remain mandatory before observe() can release this entry.
            OpenPhase::NotEntered | OpenPhase::Rejected(_) | OpenPhase::Acquired => {}
        }
        if self.native_return.is_none() {
            if let Some(pcm) = self.pcm.take() {
                match Arc::try_unwrap(pcm) {
                    Err(pcm) => {
                        self.pcm = Some(pcm);
                        return AlsaCloseObservation::Retained(AlsaCloseError::Ownership);
                    }
                    Ok(pcm) => self.native_return = Some(pcm.close()),
                }
            } else if self.open_phase == OpenPhase::Acquired {
                return AlsaCloseObservation::Retained(AlsaCloseError::Ownership);
            }
            // If native PCM was never admitted, leave native_return=None.
            // Absence of acquisition is not a synthetic snd_pcm_close(0).
        }
        if let Some(Err(status)) = self.native_return {
            // Negative status consumes the primary allocation in the reviewed
            // ABI; unexpected positive status leaves consumption unproved.
            // Either failure remains owned and is never retried.
            return AlsaCloseObservation::Retained(AlsaCloseError::Native {
                status,
                primary_handle_consumed: status < 0,
            });
        }
        if self.transport_return.is_none() {
            if self.sender_return.is_none() {
                self.sender_return = Some(
                    self.trigger
                        .take()
                        .map(TriggerSender::close_owned)
                        .unwrap_or(Ok(())),
                );
            }
            let sender = self.sender_return.expect("recorded ALSA wake sender close");
            let receiver = match self.receiver.take() {
                None => Ok(()),
                Some(receiver) => match Arc::try_unwrap(receiver) {
                    Ok(receiver) => receiver.close_owned(),
                    Err(receiver) => {
                        self.receiver = Some(receiver);
                        return AlsaCloseObservation::Retained(AlsaCloseError::Ownership);
                    }
                },
            };
            // Both independent descriptor closes are attempted. Neither fd is
            // ever retried after native return (Linux consumes it except EBADF).
            self.transport_return = Some(sender.and(receiver));
        }
        if let Some(Err(status)) = self.transport_return {
            return AlsaCloseObservation::Retained(AlsaCloseError::Transport { status });
        }
        self.latch.take();
        self.context.take();
        if self.callback_panic.is_some() {
            AlsaCloseObservation::Retained(AlsaCloseError::CallbackPanicked)
        } else {
            AlsaCloseObservation::Released
        }
    }
}

pub struct Stream {
    entry: Arc<Entry>,
}
impl std::fmt::Debug for Stream {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("AlsaOwnedStream")
    }
}
impl Stream {
    pub fn close_ticket(&self) -> AlsaCloseTicket {
        AlsaCloseTicket(self.entry.clone())
    }
    pub fn request_close(self) -> AlsaCloseTicket {
        let ticket = self.close_ticket();
        ticket.request();
        ticket
    }
    fn from_started(
        entry: Arc<Entry>,
        ready: mpsc::Receiver<Result<(), Error>>,
    ) -> Result<Self, Error> {
        match ready.recv_timeout(WAIT) {
            Ok(Ok(())) if !entry.closing.load(Ordering::Acquire) => Ok(Self { entry }),
            Ok(Err(error)) => {
                entry.request();
                Err(error)
            }
            _ => {
                entry.request();
                Err(error("ALSA startup remains owned"))
            }
        }
    }
    pub fn build_input<D, E>(
        device: Device,
        config: StreamConfig,
        format: SampleFormat,
        mut data: D,
        mut failure: E,
        timeout: Option<Duration>,
    ) -> Result<Self, Error>
    where
        D: FnMut(&crate::Data, &crate::InputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        let (entry, ready) = start(Box::new(move |resources, entry, commands, ready| {
            let setup = (|| {
                resources.context = Some(device._context.clone());
                let inner = Arc::new(device.build_stream_inner_owned(
                    resources,
                    config,
                    format,
                    alsa::Direction::Capture,
                )?);
                resources.prepare_callback(entry, inner.clone())?;
                resources.active()?;
                let receiver = resources.receiver.as_ref().expect("owned receiver").clone();
                let latch = resources.latch.as_ref().expect("owned latch").waiter();
                let callback_entry = entry.clone();
                let callback_slot = &mut resources.callback;
                if callback_slot.is_some() {
                    return Err(error("ALSA callback owner already published"));
                }
                let worker = thread::Builder::new()
                    .name("cpal-alsa-input".into())
                    .spawn(move || {
                        CALLBACK_IDS[callback_entry.index].store(current_tid(), Ordering::Release);
                        let data_entry = callback_entry.clone();
                        let failure_entry = callback_entry.clone();
                        let mut guarded_data =
                            move |data_ref: &crate::Data, info: &crate::InputCallbackInfo| {
                                callback(&data_entry, || data(data_ref, info));
                            };
                        let mut guarded_error = move |error| {
                            callback(&failure_entry, || failure(error));
                        };
                        // Declared after user-owned captures so this requests
                        // close before their destructors during worker unwind.
                        let _exit = CallbackWorkerExit(callback_entry.clone());
                        let ready = latch.wait();
                        if ready && !callback_entry.closing.load(Ordering::Acquire) {
                            super::input_stream_worker(
                                receiver,
                                &inner,
                                &mut guarded_data,
                                &mut guarded_error,
                                timeout,
                            );
                        }
                        // Normal worker exit also closes native admission. The owner
                        // still joins this exact body/captures/TLS before close.
                        callback_entry.request();
                    })
                    .map_err(|_| error("Could not start ALSA callback worker"))?;
                // Returned JoinHandle is stored immediately: no lock, Vec
                // growth, user body or other fallible operation intervenes.
                *callback_slot = Some(worker);
                let callback_thread = callback_slot
                    .as_ref()
                    .expect("published callback owner")
                    .thread()
                    .clone();
                resources
                    .latch
                    .as_mut()
                    .expect("owned latch")
                    .add_thread(callback_thread);
                Ok(())
            })();
            finish_setup(resources, entry, commands, ready, setup)
        }))?;
        Self::from_started(entry, ready)
    }
    pub fn build_output<D, E>(
        device: Device,
        config: StreamConfig,
        format: SampleFormat,
        data: D,
        mut failure: E,
        timeout: Option<Duration>,
    ) -> Result<Self, Error>
    where
        D: FnMut(&mut crate::Data, &crate::OutputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        let mut data = crate::host::monotonic_output_callback(data);
        let (entry, ready) = start(Box::new(move |resources, entry, commands, ready| {
            let setup = (|| {
                resources.context = Some(device._context.clone());
                let inner = Arc::new(device.build_stream_inner_owned(
                    resources,
                    config,
                    format,
                    alsa::Direction::Playback,
                )?);
                resources.prepare_callback(entry, inner.clone())?;
                resources.active()?;
                let receiver = resources.receiver.as_ref().expect("owned receiver").clone();
                let latch = resources.latch.as_ref().expect("owned latch").waiter();
                let callback_entry = entry.clone();
                let callback_slot = &mut resources.callback;
                if callback_slot.is_some() {
                    return Err(error("ALSA callback owner already published"));
                }
                let worker = thread::Builder::new()
                    .name("cpal-alsa-output".into())
                    .spawn(move || {
                        CALLBACK_IDS[callback_entry.index].store(current_tid(), Ordering::Release);
                        let data_entry = callback_entry.clone();
                        let failure_entry = callback_entry.clone();
                        let mut guarded_data =
                            move |data_ref: &mut crate::Data, info: &crate::OutputCallbackInfo| {
                                callback(&data_entry, || data(data_ref, info));
                            };
                        let mut guarded_error = move |error| {
                            callback(&failure_entry, || failure(error));
                        };
                        // Declared after user-owned captures so this requests
                        // close before their destructors during worker unwind.
                        let _exit = CallbackWorkerExit(callback_entry.clone());
                        let ready = latch.wait();
                        if ready && !callback_entry.closing.load(Ordering::Acquire) {
                            super::output_stream_worker(
                                receiver,
                                &inner,
                                &mut guarded_data,
                                &mut guarded_error,
                                timeout,
                            );
                        }
                        callback_entry.request();
                    })
                    .map_err(|_| error("Could not start ALSA callback worker"))?;
                // Returned JoinHandle is stored immediately: no lock, Vec
                // growth, user body or other fallible operation intervenes.
                *callback_slot = Some(worker);
                let callback_thread = callback_slot
                    .as_ref()
                    .expect("published callback owner")
                    .thread()
                    .clone();
                resources
                    .latch
                    .as_mut()
                    .expect("owned latch")
                    .add_thread(callback_thread);
                Ok(())
            })();
            finish_setup(resources, entry, commands, ready, setup)
        }))?;
        Self::from_started(entry, ready)
    }
    fn command(
        &self,
        command: impl FnOnce(mpsc::Sender<Result<(), Error>>) -> Command,
    ) -> Result<(), Error> {
        if reentrant() || self.entry.closing.load(Ordering::Acquire) {
            return Err(error("ALSA stream is closing or reentrant"));
        }
        let (tx, rx) = mpsc::channel();
        self.entry
            .commands
            .try_send(command(tx))
            .map_err(|_| error("ALSA control unavailable"))?;
        match rx.recv_timeout(WAIT) {
            Ok(result) => result,
            Err(_) => {
                self.entry.request();
                Err(error("ALSA control remains owned"))
            }
        }
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        self.entry.request();
    }
}
impl crate::traits::StreamTrait for Stream {
    fn play(&self) -> Result<(), Error> {
        self.command(Command::Play)
    }
    fn pause(&self) -> Result<(), Error> {
        self.command(Command::Pause)
    }
    fn now(&self) -> crate::StreamInstant {
        if !reentrant() && !self.entry.closing.load(Ordering::Acquire) {
            let (tx, rx) = mpsc::channel();
            if self.entry.commands.try_send(Command::Now(tx)).is_ok() {
                if let Ok(value) = rx.recv_timeout(WAIT) {
                    return value;
                }
                self.entry.request();
            }
        }
        let elapsed = self
            .entry
            .created
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed();
        crate::StreamInstant::new(elapsed.as_secs(), elapsed.subsec_nanos())
    }
    fn buffer_size(&self) -> Result<u32, Error> {
        Ok(self.entry.period.load(Ordering::Acquire))
    }
}

// ALSA invokes application callbacks on our actual Rust thread, not through
// a foreign C callback ABI. Preserve Rust unwind/JoinHandle error semantics;
// any body/capture/TLS panic is a typed retained worker failure. Payloads stay
// in Resources, never on a detached thread or a later callback/UI destructor.
struct CallbackWorkerExit(Arc<Entry>);
impl Drop for CallbackWorkerExit {
    fn drop(&mut self) {
        self.0.request();
    }
}
fn callback(entry: &Entry, body: impl FnOnce()) {
    let Some(mut admitted) = entry.enter_callback() else {
        return;
    };
    body();
    admitted.returned = true;
}

fn finish_setup(
    resources: &mut Resources,
    entry: &Arc<Entry>,
    commands: mpsc::Receiver<Command>,
    ready: mpsc::SyncSender<Result<(), Error>>,
    setup: Result<(), Error>,
) -> AlsaCloseObservation {
    match setup {
        Err(error) => {
            entry.request();
            let _ = ready.send(Err(error));
        }
        Ok(()) => {
            if ready.send(Ok(())).is_err() {
                entry.request();
            }
            while !entry.closing.load(Ordering::Acquire) {
                match commands.recv_timeout(Duration::from_millis(10)) {
                    Ok(Command::Play(reply)) => {
                        let result = resources.play();
                        if result.is_err() {
                            entry.request();
                        }
                        let _ = reply.send(result);
                    }
                    Ok(Command::Pause(reply)) => {
                        let result = resources.pause();
                        // A failed native stop/pause cannot reopen this owner.
                        if result
                            .as_ref()
                            .is_err_and(|e| e.kind() != ErrorKind::UnsupportedOperation)
                        {
                            entry.request();
                        }
                        let _ = reply.send(result);
                    }
                    Ok(Command::Now(reply)) => {
                        if !entry.closing.load(Ordering::Acquire) {
                            let _ = reply.send(resources.now());
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        entry.request();
                    }
                }
            }
        }
    }
    resources.close(entry)
}
impl Resources {
    fn prepare_callback(&mut self, entry: &Entry, inner: Arc<StreamInner>) -> Result<(), Error> {
        self.active()?;
        entry
            .period
            .store(inner.period_size as u32, Ordering::Release);
        *entry.created.lock().unwrap_or_else(|e| e.into_inner()) = inner.creation_instant;
        self.inner = Some(inner);
        // Pipe creation has no native audio side effect; the owner retains both
        // descriptors before any callback thread is admitted.
        let (sender, receiver) = super::trigger();
        self.trigger = Some(sender);
        self.receiver = Some(receiver);
        self.latch = Some(Latch::new());
        self.active()
    }
    fn play(&mut self) -> Result<(), Error> {
        self.active()?;
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| error("ALSA stream unavailable"))?;
        let state = inner.handle.state();
        self.active()?;
        match state {
            alsa::pcm::State::Prepared if inner.direction == crate::DeviceDirection::Input => {
                inner.handle.start()?
            }
            alsa::pcm::State::Paused => inner.handle.pause(false)?,
            _ => {}
        }
        self.active()?;
        self.latch.as_ref().expect("owned latch").release();
        Ok(())
    }
    fn pause(&mut self) -> Result<(), Error> {
        self.active()?;
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| error("ALSA stream unavailable"))?;
        let params = inner.handle.hw_params_current()?;
        self.active()?;
        if !params.can_pause() {
            return Err(ErrorKind::UnsupportedOperation.into());
        }
        let state = inner.handle.state();
        self.active()?;
        if state != alsa::pcm::State::Paused {
            inner.handle.pause(true)?;
        }
        self.active()
    }
    fn now(&self) -> crate::StreamInstant {
        let Some(inner) = self.inner.as_ref() else {
            return crate::StreamInstant::ZERO;
        };
        if inner.timestamp_mode != super::TimestampMode::CreationInstant {
            let kind = match inner.timestamp_mode {
                super::TimestampMode::AudioLink => alsa::pcm::AudioTstampType::LinkSynchronized,
                _ => alsa::pcm::AudioTstampType::Compat,
            };
            if let Ok(status) = alsa::pcm::StatusBuilder::new()
                .audio_htstamp_config(kind, false)
                .build(&inner.handle)
            {
                return inner.callback_instant(&status);
            }
        }
        let elapsed = inner.creation_instant.elapsed();
        crate::StreamInstant::new(elapsed.as_secs(), elapsed.subsec_nanos())
    }
}

pub(super) fn probe(
    device: Device,
    direction: alsa::Direction,
) -> Result<Vec<crate::SupportedStreamConfigRange>, Error> {
    let (output, result) = mpsc::sync_channel(1);
    let (entry, ready) = start(Box::new(move |resources, entry, _commands, ready| {
        resources.context = Some(device._context.clone());
        let queried = device
            .supported_configs_owned(resources, direction)
            .map(Iterator::collect::<Vec<_>>);
        entry.request();
        // Only plain config ranges/ErrorKind/Cow<str> cross this channel; no
        // native resource or user callback. A prior query error stays available
        // even if checked close blocks/fails. Successful capability output is
        // still withheld from the caller until the actual close/joins below.
        let _ = output.send(queried);
        let closed = resources.close(entry);
        let _ = ready.send(Ok(()));
        closed
    }))?;
    let _ = ready.recv_timeout(WAIT);
    let ticket = AlsaCloseTicket(entry);
    let observed = ticket.wait(WAIT);
    let queried = result.try_recv().ok();
    match (observed, queried) {
        (AlsaCloseObservation::Released, Some(result)) => result,
        // Reporting the first operation failure does not release or remove the
        // failed native owner. Subsequent native/update drain still fails.
        (_, Some(Err(original))) => Err(original),
        (AlsaCloseObservation::Pending, _) => {
            ticket.request();
            Err(error("ALSA capability probe remains owned"))
        }
        (AlsaCloseObservation::Retained(_), _) => {
            Err(error("ALSA capability probe cleanup failed"))
        }
        (AlsaCloseObservation::Released, None) => Err(error("ALSA probe result unavailable")),
    }
}

/// Observe only retained owners. Pending reobserves their exact workers; it
/// never starts another close or retries a consumed/failed native handle.
pub fn drain_owned(timeout: Duration) -> Result<(), AlsaCloseError> {
    drain(true, timeout)
}
pub fn drain_retired(timeout: Duration) -> Result<(), AlsaCloseError> {
    drain(false, timeout)
}
fn drain(full: bool, timeout: Duration) -> Result<(), AlsaCloseError> {
    let until = Instant::now().checked_add(timeout);
    loop {
        match observe_all(full) {
            AlsaCloseObservation::Released => return Ok(()),
            AlsaCloseObservation::Retained(error) => return Err(error),
            AlsaCloseObservation::Pending => {}
        }
        if until.is_none_or(|until| Instant::now() >= until) {
            return Err(AlsaCloseError::Pending);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    static SERIAL: Mutex<()> = Mutex::new(());
    struct HeldTls(mpsc::SyncSender<()>, mpsc::Receiver<()>);
    impl Drop for HeldTls {
        fn drop(&mut self) {
            let _ = self.0.send(());
            let _ = self.1.recv();
        }
    }
    thread_local! { static HELD_TLS: RefCell<Option<HeldTls>> = const { RefCell::new(None) }; }

    #[test]
    fn close_receipt_requires_actual_callback_tls_and_owner_join() {
        let _serial = SERIAL.lock().unwrap();
        let (entered, in_tls) = mpsc::sync_channel(1);
        let (release, released) = mpsc::channel();
        let (entry, ready) = start(Box::new(move |resources, entry, _commands, ready| {
            // No native device: this case exercises the real retained callback
            // and control/supervisor joins, not a hardware disposal claim.
            resources.callback = Some(thread::spawn(move || {
                HELD_TLS.with(|slot| *slot.borrow_mut() = Some(HeldTls(entered, released)));
            }));
            let _ = ready.send(Ok(()));
            resources.close(entry)
        }))
        .unwrap();
        ready.recv_timeout(WAIT).unwrap().unwrap();
        in_tls.recv_timeout(WAIT).unwrap();
        let ticket = AlsaCloseTicket(entry);
        assert_eq!(ticket.observe(), AlsaCloseObservation::Pending);
        release.send(()).unwrap();
        assert_eq!(ticket.wait(WAIT), AlsaCloseObservation::Released);
        assert_eq!(observe_alsa_shutdown(), AlsaCloseObservation::Released);
    }

    #[test]
    fn null_pcm_close_receipt_requires_actual_control_tls_join() {
        let _serial = SERIAL.lock().unwrap();
        let (entered, in_tls) = mpsc::sync_channel(1);
        let (release, released) = mpsc::channel();
        let (entry, ready) = start(Box::new(move |resources, entry, _commands, ready| {
            // ALSA's standard null plugin opens no hardware/account/service.
            // It still executes the real consuming snd_pcm_close path.
            let opened = resources.open("null", alsa::Direction::Playback);
            let result = opened.map(drop);
            if result.is_ok() {
                HELD_TLS.with(|slot| *slot.borrow_mut() = Some(HeldTls(entered, released)));
            }
            let _ = ready.send(result);
            resources.close(entry)
        }))
        .unwrap();
        ready.recv_timeout(WAIT).unwrap().unwrap();
        in_tls.recv_timeout(WAIT).unwrap();
        let ticket = AlsaCloseTicket(entry);
        assert_eq!(ticket.observe(), AlsaCloseObservation::Pending);
        release.send(()).unwrap();
        assert_eq!(ticket.wait(WAIT), AlsaCloseObservation::Released);
    }

    fn local_entry() -> Entry {
        let (commands, _unused) = mpsc::sync_channel(1);
        let closing = Arc::new(AtomicBool::new(false));
        Entry {
            index: 0,
            closing: closing.clone(),
            admission: AtomicU8::new(0),
            commands,
            resources: Mutex::new(Resources::new(closing)),
            result: Mutex::new(None),
            changed: Condvar::new(),
            supervisor: Mutex::new(None),
            reap: Mutex::new(()),
            panic: Mutex::new(None),
            period: AtomicU32::new(0),
            created: Mutex::new(Instant::now()),
        }
    }
    #[test]
    fn negative_consumed_close_stays_owned_but_rejected_open_transfers_no_pcm() {
        // Inert native-status injection into the actual cleanup method: this
        // does not claim that a real plugin/hardware error was induced.
        let entry = local_entry();
        let mut resources = Resources::new(entry.closing.clone());
        resources.native_return = Some(Err(-libc::EIO));
        let expected = AlsaCloseObservation::Retained(AlsaCloseError::Native {
            status: -libc::EIO,
            primary_handle_consumed: true,
        });
        assert_eq!(resources.close(&entry), expected);
        assert_eq!(resources.close(&entry), expected);
        assert_eq!(resources.native_return, Some(Err(-libc::EIO)));
        assert!(entry.closing.load(Ordering::Acquire));
        assert!(entry.enter_callback().is_none());
        let entry = local_entry();
        let mut resources = Resources::new(entry.closing.clone());
        resources.open_phase = OpenPhase::Rejected(-libc::EBUSY);
        assert_eq!(resources.close(&entry), AlsaCloseObservation::Released);
        assert!(resources.native_return.is_none());
        assert!(resources.pcm.is_none());
        // An interrupted or out-of-contract open is still not a rejection.
        for phase in [OpenPhase::Entered, OpenPhase::Rejected(0)] {
            resources.open_phase = phase;
            assert!(matches!(
                resources.close(&entry),
                AlsaCloseObservation::Retained(AlsaCloseError::Open { .. })
            ));
        }
    }
    #[test]
    fn revocation_is_atomic_with_callback_admission_and_pre_native_setup() {
        let entry = local_entry();
        let admitted = entry.enter_callback().unwrap();
        entry.request();
        assert!(entry.enter_callback().is_none());
        drop(admitted);
        assert!(entry.enter_callback().is_none());
        let resources = Resources::new(entry.closing.clone());
        assert!(resources.active().is_err());
        assert!(super::super::ensure_admitted(Some(&entry.closing)).is_err());
    }
    #[test]
    fn actual_callback_panic_is_a_retained_worker_failure_not_a_drop_receipt() {
        let _serial = SERIAL.lock().unwrap();
        let entry = local_entry();
        let mut resources = Resources::new(entry.closing.clone());
        resources.callback = Some(thread::spawn(|| panic!("synthetic ALSA callback panic")));
        assert_eq!(
            resources.close(&entry),
            AlsaCloseObservation::Retained(AlsaCloseError::CallbackPanicked)
        );
        assert!(resources.callback.is_none());
        assert!(resources.callback_panic.is_some());
        assert_eq!(resources.native_return, None);
        assert!(entry.enter_callback().is_none());
    }
    #[test]
    fn callback_unwind_revokes_before_a_later_native_admission() {
        let entry = local_entry();
        callback(&entry, || {});
        assert!(!entry.closing.load(Ordering::Acquire));
        let failed = catch_unwind(AssertUnwindSafe(|| {
            callback(&entry, || panic!("synthetic ALSA application callback"))
        }));
        assert!(failed.is_err());
        assert!(entry.closing.load(Ordering::Acquire));
        assert!(entry.enter_callback().is_none());
        assert!(Resources::new(entry.closing.clone()).active().is_err());
        // Even an out-of-contract positive close status is not rewritten to
        // zero/success. The real consuming wrapper passes its exact status.
        let mut resources = Resources::new(entry.closing.clone());
        resources.native_return = Some(Err(1));
        assert_eq!(
            resources.close(&entry),
            AlsaCloseObservation::Retained(AlsaCloseError::Native {
                status: 1,
                primary_handle_consumed: false
            })
        );
    }
    #[test]
    fn rejected_plugin_open_waits_for_actual_tls_then_allows_a_null_operation() {
        let _serial = SERIAL.lock().unwrap();
        for direction in [alsa::Direction::Capture, alsa::Direction::Playback] {
            let (entered, in_tls) = mpsc::sync_channel(1);
            let (release, released) = mpsc::channel();
            let (entry, ready) = start(Box::new(move |resources, entry, _commands, ready| {
                // Standard built-in plug delegates to a deliberately absent
                // slave. No device/account/service/config mutation is involved.
                let result = resources
                    .open("plug:SLAVE=morpheus_missing_synthetic_pcm", direction)
                    .map(drop);
                if matches!(resources.open_phase, OpenPhase::Rejected(status) if status < 0) {
                    HELD_TLS.with(|slot| *slot.borrow_mut() = Some(HeldTls(entered, released)));
                }
                let _ = ready.send(result);
                resources.close(entry)
            }))
            .unwrap();
            let operation = ready.recv_timeout(WAIT);
            let reached_tls = in_tls.recv_timeout(WAIT);
            let ticket = AlsaCloseTicket(entry.clone());
            ticket.request();
            let before_join = ticket.observe();
            let replacement = start(Box::new(|resources, entry, _commands, ready| {
                let _ = ready.send(Ok(()));
                resources.close(entry)
            }));
            let refused_replacement = replacement.is_err();
            // Drain the actual retained owner before assertions, also on an
            // unexpected operation/setup outcome. No synthetic exit receipt.
            let _ = release.send(());
            let after_join = ticket.wait(WAIT);
            if let Ok((other, answer)) = replacement {
                let _ = answer.recv_timeout(WAIT);
                assert_eq!(
                    AlsaCloseTicket(other).wait(WAIT),
                    AlsaCloseObservation::Released
                );
            }
            assert!(matches!(operation, Ok(Err(_))));
            assert!(reached_tls.is_ok());
            assert_eq!(before_join, AlsaCloseObservation::Pending);
            assert!(refused_replacement);
            assert_eq!(after_join, AlsaCloseObservation::Released);
            {
                let resources = entry.resources.lock().unwrap();
                assert!(matches!(resources.open_phase, OpenPhase::Rejected(status) if status < 0));
                assert!(resources.pcm.is_none());
                assert!(resources.native_return.is_none());
            }
            // A fresh actual operation may now acquire and checked-close null.
            let (entry, ready) = start(Box::new(move |resources, entry, _commands, ready| {
                let result = resources.open("null", direction).map(drop);
                let _ = ready.send(result);
                resources.close(entry)
            }))
            .unwrap();
            let result = ready.recv_timeout(WAIT);
            let ticket = AlsaCloseTicket(entry.clone());
            ticket.request();
            let closed = ticket.wait(WAIT);
            assert!(matches!(result, Ok(Ok(()))));
            assert_eq!(closed, AlsaCloseObservation::Released);
            let resources = entry.resources.lock().unwrap();
            assert_eq!(resources.open_phase, OpenPhase::Acquired);
            assert_eq!(resources.native_return, Some(Ok(())));
        }
        assert_eq!(observe_alsa_shutdown(), AlsaCloseObservation::Released);
    }
    #[test]
    fn failed_open_preserves_original_direction_mapping_without_a_native_retry() {
        for (direction, message) in [
            (alsa::Direction::Capture, "Device does not support input"),
            (alsa::Direction::Playback, "Device does not support output"),
        ] {
            let mapped = open_error(alsa::Error::new("snd_pcm_open", libc::EINVAL), direction);
            assert_eq!(mapped.kind(), ErrorKind::UnsupportedOperation);
            assert_eq!(mapped.message(), Some(message));
        }
        for (errno, kind) in [
            (libc::EBUSY, ErrorKind::DeviceBusy),
            (libc::EACCES, ErrorKind::PermissionDenied),
            (libc::ENODEV, ErrorKind::DeviceNotAvailable),
        ] {
            assert_eq!(
                open_error(
                    alsa::Error::new("snd_pcm_open", errno),
                    alsa::Direction::Capture
                )
                .kind(),
                kind
            );
        }
    }
}
