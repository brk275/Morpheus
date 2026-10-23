//! Retained Windows audio worker ownership. Completion is published only after
//! joining the actual worker (including TLS), then joining its supervisor.
//!
//! Native state is created inside `work`, never in a movable proxy. A native
//! operation whose retirement is uncertain must call `retain_failure` while its
//! owner-local state remains alive; an observation deadline cannot free it.

use std::{
    collections::BTreeMap,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::sync_channel,
        Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use super::stream::OwnedEvent;
use crate::{Error, ErrorKind};

// Product overlap: one old microphone/output pair plus one sequential prepared
// replacement; one catalogue and one opening/format query. No endpoint creates
// a worker merely because it appeared in the (separately bounded) catalogue.
const STREAM_OWNERS: usize = 3;
const QUERY_OWNERS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WasapiCloseError {
    AdmissionClosed,
    Capacity,
    RegistryPoisoned,
    WorkerSpawn,
    WorkerPanicked,
    SupervisorPanicked,
    Apartment,
    Activation,
    Transfer,
    BufferRelease,
    Stop,
    Notification,
    NativeReference,
    TimedOut,
}

impl std::fmt::Display for WasapiCloseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::AdmissionClosed => "Windows audio admission has ended.",
            Self::Capacity => "Windows audio ownership is busy.",
            Self::RegistryPoisoned => "Windows audio ownership could not be confirmed.",
            Self::WorkerSpawn => "Windows audio worker could not start.",
            Self::WorkerPanicked => "Windows audio worker failed.",
            Self::SupervisorPanicked => "Windows audio join could not be confirmed.",
            Self::Apartment => "Windows audio apartment could not be confirmed.",
            Self::Activation => "Windows audio activation did not retire.",
            Self::Transfer => "Windows audio interface transfer did not retire.",
            Self::BufferRelease => "Windows audio buffer release could not be confirmed.",
            Self::Stop => "Windows audio stop could not be confirmed.",
            Self::Notification => "Windows audio notification release could not be confirmed.",
            Self::NativeReference => "Windows audio reference release could not be confirmed.",
            Self::TimedOut => "Windows audio release is still pending.",
        })
    }
}
impl std::error::Error for WasapiCloseError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WasapiCloseObservation {
    Pending,
    /// Actual worker and supervisor joins are complete. An original failure is
    /// retained even if all ownership was eventually retired safely.
    Released(Result<(), WasapiCloseError>),
    /// Actual ownership remains retained. This is never permission to quit the
    /// origin apartment, replace the owner, or release an installation fence.
    Retained(WasapiCloseError),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Stream,
    Query,
}

struct State {
    supervisor: Option<JoinHandle<()>>,
    supervisor_joining: bool,
    native_result: Option<Result<(), WasapiCloseError>>,
    joined: bool,
    original_failure: Option<WasapiCloseError>,
    retained_failure: Option<WasapiCloseError>,
}

type Authority = Arc<dyn Fn() -> bool + Send + Sync>;
thread_local! { static OPEN_AUTHORITY: std::cell::RefCell<Option<Authority>> = const { std::cell::RefCell::new(None) }; }

/// Bind copied product cancellation/account authority to newly created owners.
/// Existing owners retain the same predicate; no native pointer enters TLS.
pub fn with_wasapi_open_authority<T>(authority: Authority, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<Authority>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OPEN_AUTHORITY.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let previous = OPEN_AUTHORITY.with(|slot| slot.replace(Some(authority)));
    let _restore = Restore(previous);
    work()
}

struct Slot {
    authority: Option<Authority>,
    id: u64,
    kind: Kind,
    retire: AtomicBool,
    event: Arc<OwnedEvent>,
    state: Mutex<State>,
}

struct Registry {
    next: u64,
    closed: bool,
    terminal_failure: Option<WasapiCloseError>,
    slots: BTreeMap<u64, Arc<Slot>>,
}
static OWNERS: OnceLock<Mutex<Registry>> = OnceLock::new();
static ADMISSION_CLOSED: AtomicBool = AtomicBool::new(false);
fn registry() -> &'static Mutex<Registry> {
    OWNERS.get_or_init(|| {
        Mutex::new(Registry {
            next: 0,
            closed: false,
            terminal_failure: None,
            slots: BTreeMap::new(),
        })
    })
}

fn setup_error(error: WasapiCloseError) -> Error {
    Error::with_message(ErrorKind::ResourceExhausted, error.to_string())
}

#[derive(Clone)]
pub struct WasapiCloseTicket(Arc<Slot>);

#[derive(Clone)]
pub(crate) struct Control(Arc<Slot>);

impl Control {
    pub(crate) fn active(&self) -> bool {
        !ADMISSION_CLOSED.load(Ordering::Acquire)
            && !self.0.retire.load(Ordering::Acquire)
            && self.0.authority.as_ref().is_none_or(|authority| {
                match catch_unwind(AssertUnwindSafe(|| authority())) {
                    Ok(active) => active,
                    Err(_) => {
                        self.failure(WasapiCloseError::WorkerPanicked);
                        false
                    }
                }
            })
    }
    pub(crate) fn failure(&self, error: WasapiCloseError) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.original_failure.get_or_insert(error);
    }
    pub(crate) fn event(&self) -> Arc<OwnedEvent> {
        self.0.event.clone()
    }
    pub(crate) fn ticket(&self) -> WasapiCloseTicket {
        WasapiCloseTicket(self.0.clone())
    }
    pub(crate) fn request_close(&self) {
        self.ticket().request();
    }
    /// The caller must keep its owner-local native retention container alive
    /// around this nonreturning call. No native cleanup is retried here.
    pub(crate) fn retain_failure(&self, error: WasapiCloseError) -> ! {
        self.0.retire.store(true, Ordering::Release);
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.retained_failure.get_or_insert(error);
        drop(state);
        super::apartment::retain_apartment()
    }
}

impl WasapiCloseTicket {
    pub fn request(&self) {
        self.0.retire.store(true, Ordering::Release);
        // Failure to wake an entered native wait cannot become success. The
        // retained event/worker remain in the registry in either case.
        if self.0.event.signal().is_err() {
            let mut state = self
                .0
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state
                .original_failure
                .get_or_insert(WasapiCloseError::NativeReference);
        }
    }

    pub fn observe(&self) -> WasapiCloseObservation {
        let handle = {
            let mut state = match self.0.state.lock() {
                Ok(state) => state,
                Err(_) => {
                    return WasapiCloseObservation::Retained(WasapiCloseError::RegistryPoisoned)
                }
            };
            if let Some(error) = state.retained_failure {
                return WasapiCloseObservation::Retained(error);
            }
            if state.joined {
                return WasapiCloseObservation::Released(
                    state.original_failure.map_or(Ok(()), Err),
                );
            }
            if state.supervisor_joining {
                return WasapiCloseObservation::Pending;
            }
            match state.supervisor.as_ref() {
                Some(handle) if handle.is_finished() => {
                    state.supervisor_joining = true;
                    state.supervisor.take()
                }
                _ => return WasapiCloseObservation::Pending,
            }
        };
        // is_finished is only a nonblocking scheduling hint. Actual join is
        // still mandatory; no completion receipt is constructed from the hint.
        let joined = handle.map(|handle| handle.join());
        let mut state = match self.0.state.lock() {
            Ok(state) => state,
            Err(_) => return WasapiCloseObservation::Retained(WasapiCloseError::RegistryPoisoned),
        };
        state.supervisor_joining = false;
        if !matches!(joined, Some(Ok(()))) || state.native_result.is_none() {
            state.retained_failure = Some(WasapiCloseError::SupervisorPanicked);
            return WasapiCloseObservation::Retained(WasapiCloseError::SupervisorPanicked);
        }
        if let Some(Err(error)) = state.native_result {
            state.original_failure.get_or_insert(error);
        }
        state.joined = true;
        let result = state.original_failure.map_or(Ok(()), Err);
        drop(state);
        match registry().lock() {
            Ok(mut registry) => {
                if let Err(error) = result {
                    registry.terminal_failure.get_or_insert(error);
                }
                if registry
                    .slots
                    .get(&self.0.id)
                    .is_some_and(|slot| Arc::ptr_eq(slot, &self.0))
                {
                    registry.slots.remove(&self.0.id);
                }
            }
            Err(_) => return WasapiCloseObservation::Retained(WasapiCloseError::RegistryPoisoned),
        }
        WasapiCloseObservation::Released(result)
    }

    /// Observation is bounded; neither timeout nor dropping this ticket removes
    /// the actual worker/supervisor from the registry.
    pub fn wait(&self, bound: Duration) -> Result<(), WasapiCloseError> {
        let deadline = Instant::now()
            .checked_add(bound)
            .ok_or(WasapiCloseError::TimedOut)?;
        loop {
            match self.observe() {
                WasapiCloseObservation::Released(result) => return result,
                WasapiCloseObservation::Retained(error) => return Err(error),
                WasapiCloseObservation::Pending => {}
            }
            if Instant::now() >= deadline {
                let mut state = self
                    .0
                    .state
                    .lock()
                    .map_err(|_| WasapiCloseError::RegistryPoisoned)?;
                state
                    .original_failure
                    .get_or_insert(WasapiCloseError::TimedOut);
                return Err(WasapiCloseError::TimedOut);
            }
            thread::park_timeout(Duration::from_millis(5));
        }
    }
}

pub(crate) fn spawn(
    kind: Kind,
    work: impl FnOnce(Control) -> Result<(), WasapiCloseError> + Send + 'static,
) -> Result<Control, Error> {
    // Polling old owners performs only already-finished supervisor joins.
    let _ = observe_retiring();
    let event = Arc::new(OwnedEvent::new().map_err(Error::from)?);
    let slot = {
        let mut registry = registry()
            .lock()
            .map_err(|_| setup_error(WasapiCloseError::RegistryPoisoned))?;
        if ADMISSION_CLOSED.load(Ordering::Acquire)
            || registry.closed
            || registry.terminal_failure.is_some()
        {
            return Err(setup_error(WasapiCloseError::AdmissionClosed));
        }
        if registry.slots.values().any(|slot| {
            slot.retire.load(Ordering::Acquire)
                || slot.state.lock().map_or(true, |state| {
                    state.retained_failure.is_some() || state.original_failure.is_some()
                })
        }) {
            return Err(setup_error(WasapiCloseError::NativeReference));
        }
        let maximum = if kind == Kind::Stream {
            STREAM_OWNERS
        } else {
            QUERY_OWNERS
        };
        if registry
            .slots
            .values()
            .filter(|slot| slot.kind == kind)
            .count()
            >= maximum
        {
            return Err(setup_error(WasapiCloseError::Capacity));
        }
        registry.next = registry
            .next
            .checked_add(1)
            .ok_or_else(|| setup_error(WasapiCloseError::Capacity))?;
        let authority = OPEN_AUTHORITY.with(|slot| slot.borrow().clone());
        let slot = Arc::new(Slot {
            authority,
            id: registry.next,
            kind,
            retire: AtomicBool::new(false),
            event,
            state: Mutex::new(State {
                supervisor: None,
                supervisor_joining: false,
                native_result: None,
                joined: false,
                original_failure: None,
                retained_failure: None,
            }),
        });
        registry.slots.insert(slot.id, slot.clone());
        slot
    };
    let (start, started) = sync_channel(1);
    let supervised = slot.clone();
    let supervisor = thread::Builder::new()
        .name("cpal_wasapi_join".into())
        .spawn(move || {
            if started.recv().is_err() {
                return;
            }
            let actual = supervised.clone();
            let worker = thread::Builder::new()
                .name("cpal_wasapi_owner".into())
                .spawn(move || {
                    let control = Control(actual.clone());
                    // Native retention containers inside work must catch/hold unsafe
                    // partial state before unwinding; this outer catch preserves the
                    // typed panic result and real worker/TLS join, not native proof.
                    catch_unwind(AssertUnwindSafe(|| work(control)))
                        .unwrap_or(Err(WasapiCloseError::WorkerPanicked))
                });
            let result = match worker {
                Ok(worker) => worker
                    .join()
                    .unwrap_or(Err(WasapiCloseError::WorkerPanicked)),
                Err(_) => Err(WasapiCloseError::WorkerSpawn),
            };
            supervised.retire.store(true, Ordering::Release);
            let mut state = supervised
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.native_result = Some(result);
        });
    match supervisor {
        Ok(supervisor) => {
            // The actual supervisor is retained before the gate can create a
            // native worker. A failed supervisor spawn cannot detach one.
            slot.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .supervisor = Some(supervisor);
            if start.send(()).is_err() {
                let mut state = slot.state.lock().unwrap_or_else(|error| error.into_inner());
                state.original_failure = Some(WasapiCloseError::SupervisorPanicked);
                return Err(setup_error(WasapiCloseError::SupervisorPanicked));
            }
            Ok(Control(slot))
        }
        Err(_) => {
            let mut registry = registry()
                .lock()
                .map_err(|_| setup_error(WasapiCloseError::RegistryPoisoned))?;
            registry.slots.remove(&slot.id);
            Err(setup_error(WasapiCloseError::WorkerSpawn))
        }
    }
}

pub(crate) fn query<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    let (send, receive) = sync_channel(1);
    let control = spawn(Kind::Query, move |control| {
        let result = super::apartment::Sta::new()
            .map_err(|_| setup_error(WasapiCloseError::Apartment))
            .and_then(|apartment| {
                let result = work();
                // Every operation-local native reference is dropped by work;
                // the retained COM lease is balanced on this exact worker.
                drop(apartment);
                result
            });
        control.request_close();
        let _ = send.send(result);
        Ok(())
    })?;
    let result = receive
        .recv()
        .map_err(|_| setup_error(WasapiCloseError::WorkerPanicked))?;
    // Catalogue/result delivery is not TLS completion. This is a blocking
    // query API used on retained app workers, never a UI/Tokio wait.
    control
        .ticket()
        .wait(Duration::from_secs(3))
        .map_err(setup_error)?;
    result
}

pub fn begin_shutdown() {
    // One atomic authority revocation covers already queued Start/activation
    // across all workers before the per-owner wakeup iteration begins.
    ADMISSION_CLOSED.store(true, Ordering::Release);
    let slots = match registry().lock() {
        Ok(mut registry) => {
            registry.closed = true;
            registry.slots.values().cloned().collect::<Vec<_>>()
        }
        Err(_) => return,
    };
    for slot in slots {
        WasapiCloseTicket(slot).request();
    }
}

pub fn observe_retiring() -> WasapiCloseObservation {
    observe(false)
}
pub fn observe_shutdown() -> WasapiCloseObservation {
    observe(true)
}
fn observe(all: bool) -> WasapiCloseObservation {
    let (slots, terminal_failure) = match registry().lock() {
        Ok(registry) => (
            registry
                .slots
                .values()
                .filter(|slot| all || slot.retire.load(Ordering::Acquire))
                .cloned()
                .collect::<Vec<_>>(),
            registry.terminal_failure,
        ),
        Err(_) => return WasapiCloseObservation::Retained(WasapiCloseError::RegistryPoisoned),
    };
    let mut pending = false;
    let mut failure = terminal_failure;
    for slot in slots {
        match WasapiCloseTicket(slot).observe() {
            WasapiCloseObservation::Pending => pending = true,
            WasapiCloseObservation::Retained(error) => {
                return WasapiCloseObservation::Retained(error)
            }
            WasapiCloseObservation::Released(Err(error)) => {
                failure.get_or_insert(error);
            }
            WasapiCloseObservation::Released(Ok(())) => {}
        }
    }
    if pending {
        WasapiCloseObservation::Pending
    } else {
        WasapiCloseObservation::Released(failure.map_or(Ok(()), Err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static TEST_OWNER: Mutex<()> = Mutex::new(());
    struct HeldTls(std::sync::mpsc::Receiver<()>);
    impl Drop for HeldTls {
        fn drop(&mut self) {
            let _ = self.0.recv();
        }
    }
    thread_local! { static TLS_OWNER: std::cell::RefCell<Option<HeldTls>> = const { std::cell::RefCell::new(None) }; }
    fn finish(ticket: &WasapiCloseTicket) -> WasapiCloseObservation {
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let result = ticket.observe();
            if !matches!(result, WasapiCloseObservation::Pending) || Instant::now() >= until {
                return result;
            }
            thread::park_timeout(Duration::from_millis(5));
        }
    }
    #[test]
    fn native_owner_receipt_waits_for_actual_worker_tls_and_supervisor_join() {
        let _serial = TEST_OWNER.lock().unwrap_or_else(|error| error.into_inner());
        let (release, held) = std::sync::mpsc::channel();
        let (body, observed) = std::sync::mpsc::sync_channel(1);
        let started = spawn(Kind::Query, move |control| {
            let apartment = super::super::apartment::Sta::new()?;
            TLS_OWNER.with(|slot| *slot.borrow_mut() = Some(HeldTls(held)));
            control.request_close();
            drop(apartment);
            let _ = body.send(());
            Ok(())
        });
        let observations = started.as_ref().map(|control| {
            let body = observed.recv_timeout(Duration::from_secs(3));
            (body.is_ok(), control.ticket().observe())
        });
        // Release any installed TLS owner on EVERY observation/error path
        // before assertions. Unknown cleanup retains the registry/real handles.
        let _ = release.send(());
        let retired = started.as_ref().map(|control| finish(&control.ticket()));
        assert!(matches!(
            observations,
            Ok((true, WasapiCloseObservation::Pending))
        ));
        assert!(matches!(
            retired,
            Ok(WasapiCloseObservation::Released(Ok(())))
        ));
    }
    #[test]
    fn finite_native_owner_capacity_is_refusal_and_not_a_release_receipt() {
        let _serial = TEST_OWNER.lock().unwrap_or_else(|error| error.into_inner());
        let mut owners = Vec::new();
        let mut failure = None;
        for _ in 0..STREAM_OWNERS {
            match spawn(Kind::Stream, |control| {
                while control.active() {
                    thread::park_timeout(Duration::from_millis(5));
                }
                Ok(())
            }) {
                Ok(owner) => owners.push(owner),
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        let extra = spawn(Kind::Stream, |_| Ok(()));
        let refused = extra.is_err();
        if let Ok(owner) = extra.as_ref() {
            owner.request_close();
        }
        for owner in &owners {
            owner.request_close();
        }
        let released = owners
            .iter()
            .map(|owner| finish(&owner.ticket()))
            .collect::<Vec<_>>();
        let extra_released = extra.as_ref().map(|owner| finish(&owner.ticket()));
        assert!(
            extra_released.is_err()
                || matches!(extra_released, Ok(WasapiCloseObservation::Released(Ok(()))))
        );
        assert!(failure.is_none());
        assert_eq!(owners.len(), STREAM_OWNERS);
        assert!(refused);
        assert!(released
            .iter()
            .all(|value| matches!(value, WasapiCloseObservation::Released(Ok(())))));
    }
}
