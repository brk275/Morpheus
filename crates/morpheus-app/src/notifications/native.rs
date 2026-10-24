//! The process's finite native notification owners. A body/flag/count is never
//! a worker receipt: the exact std thread (including TLS) and then its retained
//! Tokio supervisor must both really join. No native operation runs on Tokio.

use super::{NotificationDrainFailure, presenter::PresenterEvent};
use matrix_sdk::ruma::TransactionId;
use notify_rust::{
    NotificationResponse,
    owned::{Cancellation, Failure, Identity},
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Duration,
};
use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

const LISTENERS: usize = 8;
// The existing message and incoming actors each submit one banner at a time.
// With the eight listener owners these two no-listener classes give ten.
const OWNERS: usize = LISTENERS + 2;
const DRAIN: Duration = Duration::from_secs(10);
const STEP: Duration = Duration::from_millis(10);
static NATIVE: OnceLock<Arc<Owner>> = OnceLock::new();

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Source {
    Message,
    Incoming,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Class {
    Listener,
    Submission(Source),
}

#[derive(Clone)]
pub(super) struct NativeBook(Arc<Owner>);
struct Owner {
    closed: Arc<AtomicBool>,
    // Admission only: an actual returned shared/unknown native setup failure.
    // This is never a native reference release, worker, TLS or join receipt.
    shared_setup_failed: Arc<AtomicBool>,
    state: Mutex<State>,
}
impl Default for Owner {
    fn default() -> Self {
        Self {
            closed: Arc::new(AtomicBool::new(false)),
            shared_setup_failed: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(State::default()),
        }
    }
}
struct State {
    entries: [Option<Entry>; OWNERS],
    ownership_failed: bool,
    worker_failed: bool,
}
impl Default for State {
    fn default() -> Self {
        Self {
            entries: std::array::from_fn(|_| None),
            ownership_failed: false,
            worker_failed: false,
        }
    }
}
struct Entry {
    identity: Identity,
    source: Source,
    class: Class,
    cancellation: Cancellation,
    gate: Arc<ResponseGate>,
    submitted: Arc<AtomicBool>,
    // Installed before the supervisor can run or the native body can start.
    // Only the running supervisor takes this exact actual std JoinHandle.
    transfer: Arc<Mutex<Option<std::thread::JoinHandle<WorkerResult>>>>,
    supervisor: Option<JoinHandle<Joined>>,
    returned: bool,
    reserve: bool,
    native_failure: Option<Failure>,
}
// Its private constructor below is reached only AFTER std::JoinHandle::join.
struct Joined(Result<WorkerResult, ()>);
pub(super) struct WorkerResult {
    reserve: bool,
    native_failure: Option<Failure>,
    shared_setup_uncertain: bool,
}

#[cfg(any(target_os = "macos", test))]
fn shared_setup_stage_uncertain(slot: Option<u32>, cookie: u64, stage: Option<u32>) -> bool {
    // Same-version native ABI: StageDelegate (2) enters process-static
    // setupDelegate/sharedDelegate dispatch_once. Other valid stages refer to
    // this record's own construction/release; malformed receipts cannot prove
    // which side of that shared setup was reached.
    !slot.is_some_and(|slot| slot < OWNERS as u32)
        || cookie == 0
        || !stage.is_some_and(|stage| (1..=25).contains(&stage) && stage != 2)
}

#[cfg(target_os = "macos")]
fn shared_setup_uncertain(failure: &Failure) -> bool {
    match failure.retirement.native {
        notify_rust::owned::NativeRetirement::Retained(record) => shared_setup_stage_uncertain(
            record.slot,
            record.cookie,
            record.stage.map(|stage| stage.code()),
        ),
        _ => false,
    }
}
#[cfg(not(target_os = "macos"))]
fn shared_setup_uncertain(_failure: &Failure) -> bool {
    false
}
impl WorkerResult {
    pub(super) fn backend(result: Result<notify_rust::owned::Report, Failure>) -> Self {
        match result {
            Ok(report) => Self {
                reserve: report.requires_identity_reservation(),
                native_failure: None,
                shared_setup_uncertain: false,
            },
            Err(failure) => Self {
                reserve: failure.requires_identity_reservation(),
                native_failure: notify_rust::owned::retains_native_owner(&failure)
                    .then_some(failure),
                shared_setup_uncertain: shared_setup_uncertain(&failure),
            },
        }
    }
    pub(super) fn released_without_entry() -> Self {
        Self {
            reserve: false,
            native_failure: None,
            shared_setup_uncertain: false,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Submission {
    Submitted,
    Refused,
    Unavailable,
}
/// The send result is independent from early click state. No response can
/// manufacture successful submission, and no unsubmitted token can publish.
struct Reply {
    submitted: bool,
    published: bool,
    closed: bool,
    early: Option<PresenterEvent>,
    events: Option<mpsc::Sender<PresenterEvent>>,
}
struct ResponseGate {
    token: u64,
    terminal: Arc<AtomicBool>,
    reply: Mutex<Reply>,
}
impl ResponseGate {
    fn submitted(&self) {
        self.reply
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .submitted = true;
    }
    fn response(&self, response: NotificationResponse) {
        let event = if matches!(
            response,
            NotificationResponse::Default | NotificationResponse::Action(_)
        ) {
            PresenterEvent::Activated(self.token)
        } else {
            PresenterEvent::Closed(self.token)
        };
        self.event(event);
    }
    fn event(&self, event: PresenterEvent) {
        let send = {
            let mut reply = self.reply.lock().unwrap_or_else(|e| e.into_inner());
            if reply.closed || self.terminal.load(Ordering::Acquire) {
                return;
            }
            // At most one actual response, even before Submitted publication.
            if !reply.published {
                if reply.early.is_none() {
                    reply.early = Some(event);
                }
                return;
            }
            reply.closed = true;
            reply.events.take().map(|events| (events, event))
        };
        if let Some((events, event)) = send {
            let _ = events.try_send(event);
        }
    }
    fn publish(&self) {
        let send = {
            let mut reply = self.reply.lock().unwrap_or_else(|e| e.into_inner());
            if reply.closed || !reply.submitted || self.terminal.load(Ordering::Acquire) {
                return;
            }
            reply.published = true;
            reply.early.take().and_then(|event| {
                reply.closed = true;
                reply.events.take().map(|events| (events, event))
            })
        };
        if let Some((events, event)) = send {
            let _ = events.try_send(event);
        }
    }
    fn close(&self) {
        let discarded = {
            let mut reply = self.reply.lock().unwrap_or_else(|e| e.into_inner());
            reply.closed = true;
            reply.early = None;
            reply.events.take()
        };
        drop(discarded);
    }
}

#[derive(Clone)]
pub(crate) struct Publication(Arc<ResponseGate>);
impl Publication {
    pub(crate) fn publish(self) {
        self.0.publish();
    }
}
/// Dropping leaves an already-submitted banner alone; explicit withdraw closes
/// its exact listener. A closed token never clears another OS notification.
pub(crate) struct Withdrawal(WithdrawalKind);
enum WithdrawalKind {
    Native {
        cancellation: Cancellation,
        gate: Arc<ResponseGate>,
    },
    #[cfg(test)]
    Test(oneshot::Sender<()>),
}
impl Withdrawal {
    pub(crate) fn withdraw(self) {
        match self.0 {
            WithdrawalKind::Native { cancellation, gate } => {
                gate.close();
                cancellation.cancel();
            }
            #[cfg(test)]
            WithdrawalKind::Test(send) => {
                let _ = send.send(());
            }
        }
    }
    pub(crate) fn publication(&self) -> Option<Publication> {
        match &self.0 {
            WithdrawalKind::Native { gate, .. } => Some(Publication(gate.clone())),
            #[cfg(test)]
            WithdrawalKind::Test(_) => None,
        }
    }
    #[cfg(test)]
    pub(crate) fn for_test() -> (Self, oneshot::Receiver<()>) {
        let (send, receive) = oneshot::channel();
        (Self(WithdrawalKind::Test(send)), receive)
    }
}

pub(super) struct Operation {
    cancellation: Cancellation,
    gate: Arc<ResponseGate>,
    receipt: Option<oneshot::Receiver<Submission>>,
    listener: bool,
    handed: bool,
}
pub(super) enum Admission {
    Refused,
    Handed(Option<Withdrawal>),
    Unavailable,
}
impl Operation {
    pub(super) async fn wait(mut self) -> Admission {
        let received = self
            .receipt
            .take()
            .expect("one native submission observation")
            .await;
        match received {
            Ok(Submission::Submitted) => {
                self.handed = true;
                Admission::Handed(self.listener.then(|| {
                    Withdrawal(WithdrawalKind::Native {
                        cancellation: self.cancellation.clone(),
                        gate: self.gate.clone(),
                    })
                }))
            }
            Ok(Submission::Refused) => Admission::Refused,
            _ => Admission::Unavailable,
        }
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        if !self.handed {
            self.gate.close();
            self.cancellation.cancel();
        }
    }
}

/// Native-only callbacks own opaque bounded state, never app/SDK/UI/text.
#[derive(Clone)]
pub(super) struct Callbacks {
    receipt: Arc<Mutex<Option<oneshot::Sender<Submission>>>>,
    gate: Arc<ResponseGate>,
    submitted: Arc<AtomicBool>,
    shared_setup_failed: Arc<AtomicBool>,
}
impl Callbacks {
    pub(super) fn can_submit(&self) -> bool {
        !self.gate.terminal.load(Ordering::Acquire)
            && !self.shared_setup_failed.load(Ordering::Acquire)
    }
    pub(super) fn handover(&self, handover: impl FnOnce() -> bool) -> bool {
        self.can_submit() && handover() && self.can_submit()
    }
    pub(super) fn record_result(&self, result: &WorkerResult) {
        if result.shared_setup_uncertain {
            self.shared_setup_failed.store(true, Ordering::Release);
        }
    }
    pub(super) fn submitted(&mut self) {
        self.gate.submitted();
        self.submitted.store(true, Ordering::Release);
        let send = self
            .receipt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(send) = send {
            let _ = send.send(Submission::Submitted);
        }
    }
    pub(super) fn response(&self, response: NotificationResponse) {
        self.gate.response(response);
    }
    pub(super) fn finish(&mut self, refused: bool, failed: bool) {
        let send = self
            .receipt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(send) = send {
            self.gate.close();
            let _ = send.send(if refused {
                Submission::Refused
            } else {
                Submission::Unavailable
            });
        } else if failed {
            self.gate.event(PresenterEvent::Failed(self.gate.token));
        }
    }
}

impl NativeBook {
    pub(super) fn desktop() -> Self {
        let owner = Arc::new(Owner::default());
        #[cfg(not(test))]
        if NATIVE.set(owner.clone()).is_err() {
            owner.closed.store(true, Ordering::Release);
            owner
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .ownership_failed = true;
        }
        Self(owner)
    }
    pub(super) fn close(&self) {
        self.0.close();
    }
    pub(super) fn listeners_full(&self) -> bool {
        let _ = self.0.observe();
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .entries
            .iter()
            .flatten()
            .filter(|entry| entry.class == Class::Listener)
            .count()
            >= LISTENERS
    }
    pub(super) fn launch(
        &self,
        runtime: &Handle,
        source: Source,
        require_listener: bool,
        token: u64,
        events: mpsc::Sender<PresenterEvent>,
        run: impl FnOnce(Identity, Cancellation, bool, &mut Callbacks) -> WorkerResult + Send + 'static,
    ) -> Option<Operation> {
        let _ = self.0.observe();
        let raw_identity = TransactionId::new();
        let identity = Identity::from_hex(raw_identity.as_str())?;
        let cancellation = Cancellation::new();
        let gate = Arc::new(ResponseGate {
            token,
            terminal: self.0.closed.clone(),
            reply: Mutex::new(Reply {
                submitted: false,
                published: false,
                closed: false,
                early: None,
                events: Some(events),
            }),
        });
        let submitted = Arc::new(AtomicBool::new(false));
        let (send, receive) = oneshot::channel();
        let transfer = Arc::new(Mutex::new(None));
        let (start, started) = std::sync::mpsc::sync_channel::<()>(1);
        let mut state = match self.0.state.lock() {
            Ok(state) => state,
            Err(error) => {
                let mut state = error.into_inner();
                state.ownership_failed = true;
                return None;
            }
        };
        if self.0.closed.load(Ordering::Acquire)
            || self.0.shared_setup_failed.load(Ordering::Acquire)
            || state.ownership_failed
            || state.entries.iter().flatten().any(|entry| {
                entry.identity == identity
                    || (entry.source == source
                        && !entry.returned
                        && !entry.submitted.load(Ordering::Acquire))
            })
        {
            return None;
        }
        let listener = state
            .entries
            .iter()
            .flatten()
            .filter(|entry| entry.class == Class::Listener)
            .count()
            < LISTENERS;
        if require_listener && !listener {
            return None;
        }
        let class = if listener {
            Class::Listener
        } else {
            Class::Submission(source)
        };
        if state
            .entries
            .iter()
            .flatten()
            .any(|entry| entry.class == class && class != Class::Listener)
        {
            return None;
        }
        let index = state.entries.iter().position(Option::is_none)?;
        state.entries[index] = Some(Entry {
            identity,
            source,
            class,
            cancellation: cancellation.clone(),
            gate: gate.clone(),
            submitted: submitted.clone(),
            transfer: transfer.clone(),
            supervisor: None,
            returned: false,
            reserve: false,
            native_failure: None,
        });
        drop(state);
        // No user capture or native operation is dropped/invoked under State.
        // The worker can only wait here. The actual handle and then supervisor
        // are retained in the fixed slot before start is released.
        let worker_cancel = cancellation.clone();
        let callbacks = Callbacks {
            receipt: Arc::new(Mutex::new(Some(send))),
            gate: gate.clone(),
            submitted,
            shared_setup_failed: self.0.shared_setup_failed.clone(),
        };
        let spawned = std::thread::Builder::new()
            .name("morpheus-notification".into())
            .spawn(move || {
                if started.recv().is_err() {
                    return WorkerResult::released_without_entry();
                }
                let mut callbacks = callbacks;
                let result = run(identity, worker_cancel, listener, &mut callbacks);
                // Latch before actual std-thread TLS destruction, not only when
                // the later supervisor join makes this result observable.
                callbacks.record_result(&result);
                result
            });
        let thread = match spawned {
            Ok(thread) => thread,
            Err(_) => {
                let discarded = self
                    .0
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entries[index]
                    .take();
                drop(discarded);
                return None;
            }
        };
        *transfer.lock().unwrap_or_else(|e| e.into_inner()) = Some(thread);
        let actual = transfer.clone();
        let supervisor = runtime.spawn_blocking(move || {
            let thread = actual.lock().unwrap_or_else(|e| e.into_inner()).take();
            match thread {
                Some(thread) => Joined(thread.join().map_err(|payload| {
                    std::mem::forget(payload);
                })),
                None => Joined(Err(())),
            }
        });
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries[index]
            .as_mut()
            .expect("retained native slot")
            .supervisor = Some(supervisor);
        if start.send(()).is_err() {
            cancellation.cancel();
        }
        Some(Operation {
            cancellation,
            gate,
            receipt: Some(receive),
            listener,
            handed: false,
        })
    }
    pub(super) async fn finish(&self) -> Result<(), NotificationDrainFailure> {
        self.0.finish().await
    }
}
impl Owner {
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let entries = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let cancel: Vec<_> = entries
            .entries
            .iter()
            .flatten()
            .map(|entry| (entry.cancellation.clone(), entry.gate.clone()))
            .collect();
        drop(entries);
        for (cancellation, gate) in cancel {
            gate.close();
            cancellation.cancel();
        }
    }
    fn observe(&self) -> Result<(), NotificationDrainFailure> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(error) => {
                let mut state = error.into_inner();
                state.ownership_failed = true;
                state
            }
        };
        if self.shared_setup_failed.load(Ordering::Acquire) {
            state.ownership_failed = true;
        }
        let mut cx = Context::from_waker(Waker::noop());
        let mut removed = Vec::new();
        for index in 0..OWNERS {
            let Some(entry) = state.entries[index].as_mut() else {
                continue;
            };
            let Some(supervisor) = entry.supervisor.as_mut() else {
                continue;
            };
            let result = match Pin::new(supervisor).poll(&mut cx) {
                Poll::Pending => continue,
                Poll::Ready(result) => result,
            };
            entry.supervisor.take(); // Ready exactly once, never re-await.
            entry.returned = true;
            // A private Joined receipt requires the same transfer slot to have
            // been consumed. A contradictory retained handle is never freed.
            let transfer_unresolved = entry
                .transfer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some();
            match result {
                Ok(Joined(Ok(result))) => {
                    entry.reserve =
                        result.reserve || result.native_failure.is_some() || transfer_unresolved;
                    entry.native_failure = result.native_failure;
                    if !entry.reserve {
                        removed.push(index);
                    }
                    if transfer_unresolved {
                        state.ownership_failed = true;
                    }
                }
                Ok(Joined(Err(()))) => {
                    // The real std join failed. Native unwind stages are not a
                    // clean receipt: keep this same identity and actual record.
                    entry.reserve = true;
                    state.worker_failed = true;
                    state.ownership_failed = true;
                }
                Err(error) => {
                    if error.is_panic() {
                        std::mem::forget(error.into_panic());
                    }
                    entry.reserve = true;
                    state.ownership_failed = true;
                }
            }
        }
        let discarded: Vec<_> = removed
            .into_iter()
            .filter_map(|index| state.entries[index].take())
            .collect();
        let pending = state.entries.iter().flatten().any(|entry| !entry.returned);
        let native = state
            .entries
            .iter()
            .flatten()
            .any(|entry| entry.native_failure.is_some());
        let outcome = if state.ownership_failed || native {
            Err(NotificationDrainFailure::Ownership)
        } else if pending {
            Err(NotificationDrainFailure::Pending)
        } else if state.worker_failed {
            Err(NotificationDrainFailure::Worker)
        } else {
            Ok(())
        };
        drop(state);
        drop(discarded);
        outcome
    }
    async fn finish(&self) -> Result<(), NotificationDrainFailure> {
        self.close();
        tokio::time::timeout(DRAIN, async {
            loop {
                match self.observe() {
                    Err(NotificationDrainFailure::Pending) => tokio::time::sleep(STEP).await,
                    outcome => return outcome,
                }
            }
        })
        .await
        .unwrap_or(Err(NotificationDrainFailure::Pending))
    }
}
impl Drop for NativeBook {
    fn drop(&mut self) { /* Production's exact owner remains in NATIVE. */
    }
}

pub(crate) fn shutdown_native() {
    if let Some(owner) = NATIVE.get() {
        owner.close();
    }
}
pub(crate) fn observe_retained_native_work() -> Result<(), NotificationDrainFailure> {
    match NATIVE.get() {
        Some(owner) => {
            owner.close();
            owner.observe()
        }
        None => Ok(()),
    }
}
pub(crate) async fn finish_native_work() -> Result<(), NotificationDrainFailure> {
    match NATIVE.get() {
        Some(owner) => owner.finish().await,
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, sync::mpsc as std_mpsc};

    fn gate(token: u64) -> (Arc<ResponseGate>, mpsc::Receiver<PresenterEvent>) {
        let (events, receive) = mpsc::channel(4);
        (
            Arc::new(ResponseGate {
                token,
                terminal: Arc::new(AtomicBool::new(false)),
                reply: Mutex::new(Reply {
                    submitted: false,
                    published: false,
                    closed: false,
                    early: None,
                    events: Some(events),
                }),
            }),
            receive,
        )
    }

    #[test]
    fn early_response_requires_actual_submit_then_record_publication_and_is_once() {
        let (gate, mut events) = gate(7);
        gate.response(NotificationResponse::Default);
        gate.publish();
        let unsubmitted = events.try_recv();
        gate.submitted();
        let unpublished = events.try_recv();
        gate.publish();
        let positive = events.try_recv();
        gate.publish();
        gate.response(NotificationResponse::Default);
        let repeated = events.try_recv();
        let (refused, mut refused_events) = self::gate(8);
        refused.response(NotificationResponse::Default);
        refused.close();
        refused.submitted();
        refused.publish();
        let denied = refused_events.try_recv();
        assert!(unsubmitted.is_err());
        assert!(unpublished.is_err());
        assert_eq!(positive, Ok(PresenterEvent::Activated(7)));
        assert!(repeated.is_err());
        assert!(denied.is_err());
    }

    struct HeldTls {
        entered: std_mpsc::SyncSender<()>,
        release: std_mpsc::Receiver<()>,
    }
    impl Drop for HeldTls {
        fn drop(&mut self) {
            let _ = self.entered.send(());
            let _ = self.release.recv();
        }
    }
    thread_local! { static HELD: RefCell<Option<HeldTls>> = const { RefCell::new(None) }; }

    #[tokio::test]
    async fn cancelled_observation_retains_actual_std_tls_and_supervisor_until_real_join() {
        let book = NativeBook::desktop();
        let (events, _) = mpsc::channel(4);
        let (entered, entry) = std_mpsc::sync_channel(1);
        let (release, held) = std_mpsc::sync_channel(1);
        let operation = book.launch(
            &Handle::current(),
            Source::Message,
            true,
            1,
            events,
            move |_, _, _, callbacks| {
                HELD.with(|slot| {
                    *slot.borrow_mut() = Some(HeldTls {
                        entered,
                        release: held,
                    })
                });
                callbacks.submitted();
                callbacks.finish(false, false);
                WorkerResult::released_without_entry()
            },
        );
        let admission = match operation {
            Some(operation) => tokio::time::timeout(Duration::from_secs(1), operation.wait())
                .await
                .ok(),
            None => None,
        };
        // Bounded observation only. This does not release the held destructor.
        let tls_entered = entry.recv_timeout(Duration::from_secs(1)).is_ok();
        let before = book.0.observe();
        let mut stopping = Box::pin(book.finish());
        let first = std::future::poll_fn(|cx| Poll::Ready(stopping.as_mut().poll(cx))).await;
        drop(stopping); // Cancel only observation, retaining the exact owners.
        let retained = book.0.observe();
        // Release/disconnect and real joins precede every assertion.
        let _ = release.send(());
        drop(release);
        let finished = book.finish().await;
        let handed = match admission {
            Some(Admission::Handed(Some(withdrawal))) => {
                withdrawal.withdraw();
                true
            }
            _ => false,
        };
        assert!(tls_entered);
        assert!(handed);
        assert_eq!(before, Err(NotificationDrainFailure::Pending));
        assert!(first.is_pending());
        assert_eq!(retained, Err(NotificationDrainFailure::Pending));
        assert_eq!(finished, Ok(()));
    }

    #[tokio::test]
    async fn closed_admission_and_ten_retained_identities_never_start_an_extra_worker() {
        let book = NativeBook::desktop();
        let runtime = Handle::current();
        let (events, _) = mpsc::channel(16);
        let mut admitted = 0;
        let mut observations = Vec::new();
        for index in 0..OWNERS {
            let source = if index == OWNERS - 1 {
                Source::Incoming
            } else {
                Source::Message
            };
            let operation = book.launch(
                &runtime,
                source,
                false,
                index as u64,
                events.clone(),
                |_, _, _, callbacks| {
                    callbacks.submitted();
                    callbacks.finish(false, false);
                    WorkerResult {
                        reserve: true,
                        native_failure: None,
                        shared_setup_uncertain: false,
                    }
                },
            );
            if let Some(operation) = operation {
                let result = tokio::time::timeout(Duration::from_secs(1), operation.wait()).await;
                if matches!(&result, Ok(Admission::Handed(_))) {
                    admitted += 1;
                }
                observations.push(result.is_ok());
            }
        }
        let extra_entered = Arc::new(AtomicBool::new(false));
        let entered = extra_entered.clone();
        let extra = book.launch(
            &runtime,
            Source::Message,
            false,
            40,
            events.clone(),
            move |_, _, _, _| {
                entered.store(true, Ordering::Release);
                WorkerResult::released_without_entry()
            },
        );
        book.close();
        let after_close = book.launch(
            &runtime,
            Source::Incoming,
            false,
            41,
            events,
            |_, _, _, _| WorkerResult::released_without_entry(),
        );
        let finished = book.finish().await;
        let identities = book
            .0
            .state
            .lock()
            .unwrap()
            .entries
            .iter()
            .flatten()
            .count();
        assert_eq!(admitted, OWNERS);
        assert!(observations.iter().all(|observed| *observed));
        assert!(extra.is_none());
        assert!(after_close.is_none());
        assert!(!extra_entered.load(Ordering::Acquire));
        assert_eq!(finished, Ok(())); // Opaque reservations are not live workers.
        assert_eq!(identities, OWNERS);
    }

    #[tokio::test]
    async fn native_worker_panic_is_joined_once_and_same_identity_failure_stays_owned() {
        struct Payload;
        impl Drop for Payload {
            fn drop(&mut self) {
                panic!("payload must be defused");
            }
        }
        let book = NativeBook::desktop();
        let (events, _) = mpsc::channel(4);
        let operation = book.launch(
            &Handle::current(),
            Source::Message,
            true,
            1,
            events,
            |_, _, _, _| std::panic::panic_any(Payload),
        );
        let result = match operation {
            Some(operation) => tokio::time::timeout(Duration::from_secs(1), operation.wait())
                .await
                .ok(),
            None => None,
        };
        let finished = book.finish().await;
        let again = book.0.observe();
        let state = book.0.state.lock().unwrap();
        let retained = state.entries.iter().flatten().next();
        let actual_joined = retained.is_some_and(|entry| {
            entry.returned
                && entry.supervisor.is_none()
                && entry.transfer.lock().unwrap().is_none()
                && entry.reserve
        });
        drop(state);
        assert!(matches!(result, Some(Admission::Unavailable)));
        assert_eq!(finished, Err(NotificationDrainFailure::Ownership));
        assert_eq!(again, Err(NotificationDrainFailure::Ownership));
        assert!(actual_joined);
    }

    #[test]
    fn shared_setup_classifier_refuses_delegate_and_unknown_but_keeps_per_record_stages() {
        for slot in [0, 1, (OWNERS - 1) as u32] {
            for stage in 1..=25 {
                assert_eq!(
                    shared_setup_stage_uncertain(Some(slot), 1, Some(stage)),
                    stage == 2
                );
            }
        }
        for (slot, cookie, stage) in [
            (None, 1, Some(3)),
            (Some(OWNERS as u32), 1, Some(3)),
            (Some(0), 0, Some(3)),
            (Some(0), 1, None),
            (Some(0), 1, Some(0)),
            (Some(0), 1, Some(26)),
        ] {
            assert!(shared_setup_stage_uncertain(slot, cookie, stage));
        }
    }

    #[tokio::test]
    async fn shared_setup_fences_book_and_handover_before_tls_join_preserving_saved_click() {
        let book = NativeBook::desktop();
        let runtime = Handle::current();
        let (events, mut event_rx) = mpsc::channel(8);
        let (captured, capture) = std_mpsc::sync_channel(1);
        let saved = book.launch(
            &runtime,
            Source::Message,
            true,
            90,
            events.clone(),
            move |_, _, _, callbacks| {
                callbacks.submitted();
                let _ = captured.send(callbacks.clone());
                callbacks.finish(false, false);
                WorkerResult {
                    reserve: true, // Opaque identity only, no native record.
                    native_failure: None,
                    shared_setup_uncertain: false,
                }
            },
        );
        let saved_admission = match saved {
            Some(operation) => tokio::time::timeout(Duration::from_secs(1), operation.wait())
                .await
                .ok(),
            None => None,
        };
        let callbacks = capture.recv_timeout(Duration::from_secs(1)).ok();
        let saved_handed = match &saved_admission {
            Some(Admission::Handed(Some(withdrawal))) => {
                if let Some(publication) = withdrawal.publication() {
                    publication.publish();
                }
                true
            }
            _ => false,
        };
        // Observe the first real std/TLS/supervisor join. It must not be the
        // source's single-flight guard that refuses the later Message action.
        let first_joined = tokio::time::timeout(DRAIN, async {
            loop {
                let _ = book.0.observe();
                if book
                    .0
                    .state
                    .lock()
                    .unwrap()
                    .entries
                    .iter()
                    .flatten()
                    .all(|e| e.returned)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        let positive_handover = callbacks
            .as_ref()
            .is_some_and(|callbacks| callbacks.handover(|| true));
        let (entered, entry) = std_mpsc::sync_channel(1);
        let (release, held) = std_mpsc::sync_channel(1);
        let failed = book.launch(
            &runtime,
            Source::Incoming,
            true,
            91,
            events.clone(),
            move |_, _, _, callbacks| {
                HELD.with(|slot| {
                    *slot.borrow_mut() = Some(HeldTls {
                        entered,
                        release: held,
                    });
                });
                // Synthetic same private classification fact; no OS call or
                // fabricated native release receipt. Production obtains this
                // fact only from its typed returned retained-stage failure.
                let result = WorkerResult {
                    reserve: true,
                    native_failure: None,
                    shared_setup_uncertain: shared_setup_stage_uncertain(Some(0), 1, Some(2)),
                };
                callbacks.record_result(&result);
                callbacks.finish(false, true);
                result
            },
        );
        let failure_admission = match failed {
            Some(operation) => tokio::time::timeout(Duration::from_secs(1), operation.wait())
                .await
                .ok(),
            None => None,
        };
        let held_tls = entry.recv_timeout(Duration::from_secs(1)).is_ok();
        let entered_extra = Arc::new(AtomicBool::new(false));
        let extra_flag = entered_extra.clone();
        let extra = book.launch(
            &runtime,
            Source::Message,
            true,
            92,
            events,
            move |_, _, _, _| {
                extra_flag.store(true, Ordering::Release);
                WorkerResult::released_without_entry()
            },
        );
        let handover_called = AtomicBool::new(false);
        let late_handover = callbacks.as_ref().is_some_and(|callbacks| {
            callbacks.handover(|| {
                handover_called.store(true, Ordering::Release);
                true
            })
        });
        if let Some(callbacks) = &callbacks {
            callbacks.response(NotificationResponse::Default);
        }
        let saved_click = event_rx.try_recv();
        let retained_before_join = book.0.observe();
        // Release/disconnect the exact held destructor on every outcome before
        // any assertion, then observe all real original joins despite failure.
        let _ = release.send(());
        drop(release);
        let all_joined = tokio::time::timeout(DRAIN, async {
            loop {
                let _ = book.0.observe();
                if book
                    .0
                    .state
                    .lock()
                    .unwrap()
                    .entries
                    .iter()
                    .flatten()
                    .all(|e| e.returned)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        if !all_joined {
            // Terminal uncertain test failure keeps the actual owner rather
            // than dropping an unjoined handle; this is never a clean receipt.
            std::mem::forget(book.clone());
        }
        let finished = book.finish().await;
        assert!(saved_handed && first_joined && positive_handover);
        assert!(matches!(failure_admission, Some(Admission::Unavailable)));
        assert!(held_tls && all_joined);
        assert!(extra.is_none() && !entered_extra.load(Ordering::Acquire));
        assert!(!late_handover && !handover_called.load(Ordering::Acquire));
        assert_eq!(saved_click, Ok(PresenterEvent::Activated(90)));
        assert_eq!(
            retained_before_join,
            Err(NotificationDrainFailure::Ownership)
        );
        assert_eq!(finished, Err(NotificationDrainFailure::Ownership));
    }

    #[tokio::test]
    async fn released_failure_and_independent_per_record_fact_keep_real_book_admission() {
        let book = NativeBook::desktop();
        let runtime = Handle::current();
        let (events, _) = mpsc::channel(8);
        let first = book.launch(
            &runtime,
            Source::Message,
            true,
            100,
            events.clone(),
            |_, _, _, callbacks| {
                callbacks.finish(false, true); // Ordinary known-released unavailable.
                WorkerResult::released_without_entry()
            },
        );
        let first = match first {
            Some(operation) => tokio::time::timeout(Duration::from_secs(1), operation.wait())
                .await
                .ok(),
            None => None,
        };
        let per_record = book.launch(
            &runtime,
            Source::Incoming,
            true,
            101,
            events.clone(),
            |_, _, _, callbacks| {
                let result = WorkerResult {
                    reserve: true,
                    native_failure: None,
                    shared_setup_uncertain: shared_setup_stage_uncertain(Some(1), 1, Some(3)),
                };
                callbacks.record_result(&result);
                if callbacks.handover(|| true) {
                    callbacks.submitted();
                }
                callbacks.finish(false, false);
                result
            },
        );
        let per_record = match per_record {
            Some(operation) => tokio::time::timeout(Duration::from_secs(1), operation.wait())
                .await
                .ok(),
            None => None,
        };
        // Retire only actual already-returned controls, keeping the book open.
        let joined = tokio::time::timeout(DRAIN, async {
            loop {
                let _ = book.0.observe();
                if book
                    .0
                    .state
                    .lock()
                    .unwrap()
                    .entries
                    .iter()
                    .flatten()
                    .all(|e| e.returned)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        let next = book.launch(
            &runtime,
            Source::Message,
            true,
            102,
            events,
            |_, _, _, callbacks| {
                if callbacks.handover(|| true) {
                    callbacks.submitted();
                }
                callbacks.finish(false, false);
                WorkerResult::released_without_entry()
            },
        );
        let next = match next {
            Some(operation) => tokio::time::timeout(Duration::from_secs(1), operation.wait())
                .await
                .ok(),
            None => None,
        };
        let finished = book.finish().await;
        if finished == Err(NotificationDrainFailure::Pending) {
            std::mem::forget(book.clone());
        }
        assert!(matches!(first, Some(Admission::Unavailable)));
        assert!(matches!(per_record, Some(Admission::Handed(_))));
        assert!(joined && matches!(next, Some(Admission::Handed(_))));
        assert_eq!(finished, Ok(()));
    }
}
