//! Main-thread ownership for ordinary macOS cleanup. Native termination wiring
//! is separate: this module never calls AppKit, Slint quit, or a native API.
//! A clean result includes the actual Runtime-drop worker join, not just the
//! cleanup tuple or a thread-body completion flag.

use std::{
    any::Any,
    cell::{Cell, RefCell},
    future::Future,
    marker::PhantomData,
    mem,
    pin::Pin,
    rc::Rc,
    sync::{Arc, Mutex, TryLockError, mpsc},
    task::{Context, Poll, Waker},
    thread,
};
use tokio::{runtime::Runtime, task::JoinHandle};

type CleanupFuture<R> = Pin<Box<dyn Future<Output = R> + Send>>;
type Cleanup<R> = Box<dyn FnOnce() -> CleanupFuture<R>>;
type Conclusion<R> = Box<dyn FnOnce(Result<R, TaskFailure>) -> Completion>;
type FutureSlot<R> = Arc<Mutex<Option<CleanupFuture<R>>>>;
type RuntimeSlot = Arc<Mutex<Option<Runtime>>>;
type Worker = thread::JoinHandle<Result<(), Failure>>;

/// Only actual completed owner observations permit Runtime destruction.
/// Pending owners need this same Runtime for their retained tasks to finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeDisposition {
    Finalize,
    Retain,
}

pub(crate) struct OwnerOutcome {
    disposition: RuntimeDisposition,
    result: Result<(), Failure>,
}

impl OwnerOutcome {
    /// All app and native owners have completed. A genuinely joined failure
    /// still refuses clean exit, but no longer requires its executor.
    pub(crate) fn finalize(result: Result<(), Failure>) -> Self {
        Self {
            disposition: RuntimeDisposition::Finalize,
            result,
        }
    }

    /// Ownership is unproved. Keep the actual Runtime and reachable owners;
    /// this fixed failure is not a fabricated live task or a Pending receipt.
    pub(crate) fn retain(failure: Failure) -> Self {
        Self {
            disposition: RuntimeDisposition::Retain,
            result: Err(failure),
        }
    }
}

/// The origin-thread conclusion runs once, including acceptance.finish.
/// An initial Pending is represented by one actual observation future, not a
/// cached failed step. That future only re-observes the SAME retained owners;
/// it must not restart cleanup, native work, or publication side effects.
pub(crate) struct Completion {
    outcome: OwnerOutcome,
    observation: Option<CleanupFuture<OwnerOutcome>>,
    resources: Option<Rc<dyn Any>>,
}

impl Completion {
    pub(crate) fn finalize(result: Result<(), Failure>) -> Self {
        Self {
            outcome: OwnerOutcome::finalize(result),
            observation: None,
            resources: None,
        }
    }

    pub(crate) fn retain(failure: Failure) -> Self {
        Self {
            outcome: OwnerOutcome::retain(failure),
            observation: None,
            resources: None,
        }
    }

    /// Preserve the actual cleanup captures when store/native ownership is
    /// unproved. No manager.stop, fake parked task, or clean receipt is implied.
    pub(crate) fn retain_resources<T: Any>(failure: Failure, resources: T) -> Self {
        Self {
            outcome: OwnerOutcome::retain(failure),
            observation: None,
            resources: Some(Rc::new(resources)),
        }
    }

    pub(crate) fn observe(
        result: Result<(), Failure>,
        observation: impl Future<Output = OwnerOutcome> + Send + 'static,
    ) -> Self {
        Self {
            outcome: OwnerOutcome {
                disposition: RuntimeDisposition::Retain,
                result,
            },
            observation: Some(Box::pin(observation)),
            resources: None,
        }
    }
}

pub(crate) use crate::app_shutdown::{Failure, ShutdownStep};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskFailure {
    Panicked,
    Cancelled,
    Ownership,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Observation {
    Pending,
    Failed(Failure),
    Clean,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Ready,
    Starting,
    Draining,
    Concluding,
    Observing,
    Retained,
    Finalizing,
    Finished,
}

struct State<R> {
    phase: Phase,
    cleanup: Option<Cleanup<R>>,
    conclusion: Option<Conclusion<R>>,
    transfer: Option<FutureSlot<R>>,
    task: Option<JoinHandle<Result<R, TaskFailure>>>,
    observation_transfer: Option<FutureSlot<OwnerOutcome>>,
    observation_task: Option<JoinHandle<Result<OwnerOutcome, TaskFailure>>>,
    resources: Option<Rc<dyn Any>>,
    finalizer: Option<Finalizer>,
    failure: Option<Failure>,
}

/// Intentionally !Send and !Sync: UI revocation and conclusion stay on origin.
/// Own the unique Runtime only after UI/controller construction is complete.
pub(crate) struct Coordinator<R: Send + 'static> {
    state: RefCell<Option<State<R>>>,
    runtime: RuntimeSlot,
    requested: Cell<bool>,
    polling: Cell<bool>,
    ownership_failed: Cell<bool>,
    _origin: PhantomData<Rc<()>>,
}

impl<R: Send + 'static> Coordinator<R> {
    pub(crate) fn new<F>(
        runtime: Runtime,
        cleanup: impl FnOnce() -> F + 'static,
        conclusion: impl FnOnce(Result<R, TaskFailure>) -> Completion + 'static,
    ) -> Self
    where
        F: Future<Output = R> + Send + 'static,
    {
        Self {
            state: RefCell::new(Some(State {
                phase: Phase::Ready,
                cleanup: Some(Box::new(move || Box::pin(cleanup()))),
                conclusion: Some(Box::new(conclusion)),
                transfer: None,
                task: None,
                observation_transfer: None,
                observation_task: None,
                resources: None,
                finalizer: None,
                failure: None,
            })),
            runtime: Arc::new(Mutex::new(Some(runtime))),
            requested: Cell::new(false),
            polling: Cell::new(false),
            ownership_failed: Cell::new(false),
            _origin: PhantomData,
        }
    }

    /// Required integration DATA enables this same existing receipt on Mac.
    /// The controller must release its inner mutex before calling this method.
    #[cfg(target_os = "macos")]
    pub(crate) fn request_admitted(
        &self,
        _admission: crate::security::ShutdownAdmission<'_>,
    ) -> bool {
        self.start(None)
    }

    /// Only the actual post-ui.run caller uses this fallback. An unexpected
    /// loop return cannot become clean merely because later cleanup succeeds.
    pub(crate) fn after_event_loop_return(&self) {
        {
            let mut slot = self.state.borrow_mut();
            if let Some(state) = slot.as_mut() {
                if state.phase == Phase::Finished {
                    return;
                }
                state.failure = state.failure.or(Some(Failure::EventLoopEnded));
            }
        }
        self.start(Some(Failure::EventLoopEnded));
    }

    /// Ordinary quit may leave the UI loop only after this actual join. A
    /// known earlier failure can be displayed before finalization completes.
    pub(crate) fn runtime_finalized(&self) -> bool {
        self.state
            .borrow()
            .as_ref()
            .is_some_and(|state| state.phase == Phase::Finished)
    }

    /// The origin owner remains reachable on Retain. Clone under State, then
    /// downcast after releasing it; no caller code or resource Drop runs there.
    pub(crate) fn retained_resources<T: Any>(&self) -> Option<Rc<T>> {
        let resources = {
            let slot = self.state.borrow();
            slot.as_ref()?.resources.clone()
        }?;
        resources.downcast::<T>().ok()
    }

    fn start(&self, failure: Option<Failure>) -> bool {
        if self.requested.replace(true) {
            return !self.ownership_failed.get();
        }
        let cleanup = {
            let Ok(mut slot) = self.state.try_borrow_mut() else {
                self.ownership_failed.set(true);
                return false;
            };
            let Some(state) = slot.as_mut() else {
                self.ownership_failed.set(true);
                return false;
            };
            state.phase = Phase::Starting;
            state.failure = failure;
            state.cleanup.take()
        };
        let Some(cleanup) = cleanup else {
            self.ownership_failed.set(true);
            return false;
        };
        // Actual synchronous UI withdrawal happens with no State borrow held.
        let future = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(cleanup)) {
            Ok(future) => future,
            Err(payload) => {
                retain_panic(payload);
                self.conclude(Err(TaskFailure::Panicked));
                return true;
            }
        };
        let transfer = Arc::new(Mutex::new(Some(future)));
        {
            let mut slot = self.state.borrow_mut();
            let state = slot.as_mut().expect("coordinator state is retained");
            state.transfer = Some(transfer.clone());
        }
        match spawn_gated(&self.runtime, transfer) {
            Ok((task, start)) => {
                {
                    let mut slot = self.state.borrow_mut();
                    let state = slot.as_mut().expect("coordinator state is retained");
                    state.task = Some(task);
                    state.phase = Phase::Draining;
                }
                // The real handle is retained before this task may take or
                // poll the sole cleanup future. No State borrow spans send.
                if start.send(()).is_err() {
                    self.fail_before_task();
                }
            }
            Err(_) => self.fail_before_task(),
        }
        true
    }

    fn fail_before_task(&self) {
        self.ownership_failed.set(true);
        let conclusion = {
            let mut slot = self.state.borrow_mut();
            let state = slot.as_mut().expect("coordinator state is retained");
            state.failure = state.failure.or(Some(Failure::Ownership));
            state.phase = Phase::Retained;
            state.conclusion.take()
        };
        // In instrumented builds this also consumes the actual acceptance
        // finish owner. No returned value can erase the ownership failure.
        if let Some(conclusion) = conclusion {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                conclusion(Err(TaskFailure::Ownership))
            })) {
                // An unpolled cleanup is still owned. Do not drop a returned
                // continuation or let a permissive conclusion finalize it.
                Ok(completion) => mem::forget(completion),
                Err(payload) => retain_panic(payload),
            }
        }
        // Do not destroy a Runtime beneath the retained, unpolled cleanup.
    }

    /// Called by the separately owned main-thread modal/default observer.
    /// No UI callback, future poll, join or destructor runs under State borrow.
    pub(crate) fn poll(&self) -> Observation {
        if self.ownership_failed.get() {
            return Observation::Failed(Failure::Ownership);
        }
        if self.polling.replace(true) {
            return Observation::Pending;
        }
        struct Polling<'a>(&'a Cell<bool>);
        impl Drop for Polling<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _polling = Polling(&self.polling);
        let task = {
            let Ok(mut slot) = self.state.try_borrow_mut() else {
                self.ownership_failed.set(true);
                return Observation::Failed(Failure::Ownership);
            };
            slot.as_mut().and_then(|state| state.task.take())
        };
        if let Some(task) = task {
            match poll_task(task) {
                TaskPoll::Pending(task) => {
                    self.state.borrow_mut().as_mut().unwrap().task = Some(task);
                }
                TaskPoll::Ready(result) => self.conclude(result),
                TaskPoll::Unproved(task) => {
                    self.state.borrow_mut().as_mut().unwrap().task = Some(task);
                    self.ownership_failed.set(true);
                    return Observation::Failed(Failure::Ownership);
                }
            }
        }
        let observation_task = {
            let mut slot = self.state.borrow_mut();
            slot.as_mut()
                .and_then(|state| state.observation_task.take())
        };
        if let Some(task) = observation_task {
            match poll_task(task) {
                TaskPoll::Pending(task) => {
                    self.state.borrow_mut().as_mut().unwrap().observation_task = Some(task);
                }
                TaskPoll::Ready(Ok(outcome)) => self.apply_outcome(outcome),
                TaskPoll::Ready(Err(error)) => {
                    let mut slot = self.state.borrow_mut();
                    let state = slot.as_mut().unwrap();
                    state.failure = state.failure.or(Some(task_failure(error)));
                    state.phase = Phase::Retained;
                }
                TaskPoll::Unproved(task) => {
                    self.state.borrow_mut().as_mut().unwrap().observation_task = Some(task);
                    self.ownership_failed.set(true);
                    return Observation::Failed(Failure::Ownership);
                }
            }
        }
        let finalizer_observation = {
            let slot = self.state.borrow();
            slot.as_ref()
                .filter(|state| state.phase == Phase::Finalizing)
                .map(|state| state.finalizer.as_ref().map(Finalizer::observer))
        };
        // Only fixed Copy receipt data is read, outside the State borrow.
        // Even a contended receipt mutex must not block the modal/UI thread.
        if let Some(observation) = finalizer_observation {
            let observed = observation.map(|observation| observation.read());
            let mut slot = self.state.borrow_mut();
            let state = slot.as_mut().unwrap();
            match observed {
                Some(Ok(None)) => {}
                Some(Ok(Some(result))) => {
                    state.failure = state.failure.or(result.err());
                    state.phase = Phase::Finished;
                }
                Some(Err(error)) => {
                    state.failure = state.failure.or(Some(error));
                }
                None => self.ownership_failed.set(true),
            }
        }
        let slot = self.state.borrow();
        let Some(state) = slot.as_ref() else {
            return Observation::Failed(Failure::Ownership);
        };
        if self.ownership_failed.get() {
            Observation::Failed(Failure::Ownership)
        } else if let Some(failure) = state.failure {
            Observation::Failed(failure)
        } else if state.phase == Phase::Finished {
            Observation::Clean
        } else {
            Observation::Pending
        }
    }

    fn conclude(&self, result: Result<R, TaskFailure>) {
        let incomplete_cleanup = result.is_err();
        let conclusion = {
            let mut slot = self.state.borrow_mut();
            let state = slot.as_mut().expect("coordinator state is retained");
            state.phase = Phase::Concluding;
            state.failure = state
                .failure
                .or(result.as_ref().err().copied().map(task_failure));
            state.conclusion.take()
        };
        let mut result = match conclusion {
            Some(conclusion) => {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| conclusion(result)))
                {
                    Ok(result) => result,
                    Err(payload) => {
                        retain_panic(payload);
                        Completion::retain(Failure::CleanupPanicked)
                    }
                }
            }
            None => Completion::retain(Failure::Ownership),
        };
        // A joined task failure does not prove the product cleanup steps
        // completed. A permissive conclusion cannot destroy their executor.
        // It may provide one actual same-owner continuation to establish the
        // remaining receipts; the original fixed failure remains sticky.
        if incomplete_cleanup && result.observation.is_none() {
            result.outcome.disposition = RuntimeDisposition::Retain;
        }
        self.apply_completion(result);
    }

    fn apply_completion(&self, completion: Completion) {
        {
            let mut slot = self.state.borrow_mut();
            let state = slot.as_mut().unwrap();
            state.failure = state.failure.or(completion.outcome.result.err());
            // Completion is consumed once, so this slot is never replaced by
            // another resource owner or dropped under the State borrow.
            state.resources = completion.resources;
        }
        let Some(observation) = completion.observation else {
            self.apply_outcome(completion.outcome);
            return;
        };
        let transfer = Arc::new(Mutex::new(Some(observation)));
        {
            let mut slot = self.state.borrow_mut();
            let state = slot.as_mut().unwrap();
            state.observation_transfer = Some(transfer.clone());
        }
        match spawn_gated(&self.runtime, transfer) {
            Ok((task, start)) => {
                {
                    let mut slot = self.state.borrow_mut();
                    let state = slot.as_mut().unwrap();
                    state.observation_task = Some(task);
                    state.phase = Phase::Observing;
                }
                if start.send(()).is_err() {
                    self.retain_ownership_failure();
                }
            }
            Err(_) => self.retain_ownership_failure(),
        }
    }

    fn retain_ownership_failure(&self) {
        let mut slot = self.state.borrow_mut();
        let state = slot.as_mut().unwrap();
        state.failure = state.failure.or(Some(Failure::Ownership));
        state.phase = Phase::Retained;
    }

    fn apply_outcome(&self, outcome: OwnerOutcome) {
        {
            let mut slot = self.state.borrow_mut();
            let state = slot.as_mut().unwrap();
            state.failure = state.failure.or(outcome.result.err());
            if outcome.disposition == RuntimeDisposition::Retain {
                state.phase = Phase::Retained;
                return;
            }
        }
        // No EnterGuard is created or transferred. This worker cannot run on
        // the Runtime it destroys; actual finalizer join is off the UI thread.
        let finalizer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Finalizer::prepare(self.runtime.clone())
        }));
        match finalizer {
            Ok((finalizer, start)) => {
                {
                    let mut slot = self.state.borrow_mut();
                    let state = slot.as_mut().unwrap();
                    state.finalizer = Some(finalizer);
                    state.phase = Phase::Finalizing;
                }
                if let Some(start) = start {
                    if let Err(error) = start.release() {
                        let mut slot = self.state.borrow_mut();
                        let state = slot.as_mut().unwrap();
                        state.failure = state.failure.or(Some(error));
                    }
                }
            }
            Err(payload) => {
                retain_panic(payload);
                self.ownership_failed.set(true);
            }
        }
    }

    /// Ordinary post-loop code may join the small join-supervisor itself.
    /// This blocks and MUST NOT run from the main-loop/modal polling callback.
    pub(crate) fn finish_after_event_loop(&self) -> Result<(), Failure> {
        let supervisor = {
            let mut slot = self.state.borrow_mut();
            let state = slot.as_mut().ok_or(Failure::Ownership)?;
            if state.phase != Phase::Finished {
                return Err(Failure::Ownership);
            }
            state
                .finalizer
                .as_mut()
                .and_then(|finalizer| finalizer.supervisor.take())
        };
        if let Some(supervisor) = supervisor {
            if let Err(payload) = supervisor.join() {
                retain_panic(payload);
                self.ownership_failed.set(true);
                return Err(Failure::FinalizerPanicked);
            }
        }
        match self.poll() {
            Observation::Clean => Ok(()),
            Observation::Failed(error) => Err(error),
            Observation::Pending => Err(Failure::Ownership),
        }
    }
}

impl<R: Send + 'static> Drop for Coordinator<R> {
    fn drop(&mut self) {
        let state = self.state.get_mut().take();
        if let Some(state) = state {
            if state.phase != Phase::Finished
                || state.failure.is_some()
                || self.ownership_failed.get()
            {
                // Abandonment is not completion. Keep all real ownership,
                // including an unpolled future and the sole Runtime slot.
                mem::forget(state);
                mem::forget(self.runtime.clone());
            }
        }
    }
}

fn task_failure(error: TaskFailure) -> Failure {
    match error {
        TaskFailure::Panicked => Failure::CleanupPanicked,
        TaskFailure::Cancelled => Failure::CleanupCancelled,
        TaskFailure::Ownership => Failure::Ownership,
    }
}

/// The transfer slot belongs to State before spawn. A successfully spawned
/// task cannot take/poll its future until State retains the actual handle.
fn spawn_gated<T: Send + 'static>(
    runtime: &RuntimeSlot,
    transfer: FutureSlot<T>,
) -> Result<
    (
        JoinHandle<Result<T, TaskFailure>>,
        tokio::sync::oneshot::Sender<()>,
    ),
    TaskFailure,
> {
    let handle = runtime
        .lock()
        .map_err(|_| TaskFailure::Ownership)?
        .as_ref()
        .map(Runtime::handle)
        .cloned()
        .ok_or(TaskFailure::Ownership)?;
    let (start, wait) = tokio::sync::oneshot::channel();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle.spawn(async move {
            wait.await.map_err(|_| TaskFailure::Ownership)?;
            let future = transfer
                .lock()
                .map_err(|_| TaskFailure::Ownership)?
                .take()
                .ok_or(TaskFailure::Ownership)?;
            Ok(future.await)
        })
    })) {
        Ok(task) => Ok((task, start)),
        Err(payload) => {
            retain_panic(payload);
            Err(TaskFailure::Ownership)
        }
    }
}

enum TaskPoll<T> {
    Pending(JoinHandle<Result<T, TaskFailure>>),
    Ready(Result<T, TaskFailure>),
    Unproved(JoinHandle<Result<T, TaskFailure>>),
}

fn poll_task<T>(mut task: JoinHandle<Result<T, TaskFailure>>) -> TaskPoll<T> {
    let mut context = Context::from_waker(Waker::noop());
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Pin::new(&mut task).poll(&mut context)
    })) {
        Ok(Poll::Pending) => TaskPoll::Pending(task),
        Ok(Poll::Ready(result)) => TaskPoll::Ready(result.unwrap_or_else(|error| {
            if error.is_panic() {
                retain_panic(error.into_panic());
                Err(TaskFailure::Panicked)
            } else {
                Err(TaskFailure::Cancelled)
            }
        })),
        Err(payload) => {
            retain_panic(payload);
            TaskPoll::Unproved(task)
        }
    }
}

struct Finalizer {
    worker: Arc<Mutex<Option<Worker>>>,
    result: Arc<Mutex<Option<Result<(), Failure>>>>,
    supervisor: Option<thread::JoinHandle<()>>,
    start_failure: Option<Failure>,
}

struct FinalizerStart {
    worker: mpsc::Sender<()>,
    supervisor: mpsc::Sender<()>,
}

impl FinalizerStart {
    fn release(self) -> Result<(), Failure> {
        // If the supervisor has already failed, do not release the Runtime.
        self.supervisor.send(()).map_err(|_| Failure::Ownership)?;
        self.worker.send(()).map_err(|_| Failure::Ownership)
    }
}

struct FinalizerObservation {
    result: Arc<Mutex<Option<Result<(), Failure>>>>,
    start_failure: Option<Failure>,
}

impl FinalizerObservation {
    fn read(&self) -> Result<Option<Result<(), Failure>>, Failure> {
        if let Some(error) = self.start_failure {
            return Err(error);
        }
        match self.result.try_lock() {
            Ok(result) => Ok(*result),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Poisoned(_)) => Err(Failure::Ownership),
        }
    }
}

impl Finalizer {
    fn prepare(runtime: RuntimeSlot) -> (Self, Option<FinalizerStart>) {
        let mut owner = Self {
            worker: Arc::new(Mutex::new(None)),
            result: Arc::new(Mutex::new(None)),
            supervisor: None,
            start_failure: None,
        };
        let (worker_start, worker_wait) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("morpheus-runtime-close".into())
            .spawn(move || {
                worker_wait.recv().map_err(|_| Failure::Ownership)?;
                let runtime = runtime
                    .lock()
                    .map_err(|_| Failure::Ownership)?
                    .take()
                    .ok_or(Failure::Ownership)?;
                drop(runtime);
                Ok(())
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(_) => {
                owner.start_failure = Some(Failure::FinalizerStart);
                return (owner, None);
            }
        };
        *owner.worker.lock().expect("new unshared worker slot") = Some(worker);
        let worker = owner.worker.clone();
        let result = owner.result.clone();
        let (supervisor_start, supervisor_wait) = mpsc::channel();
        let supervisor = thread::Builder::new()
            .name("morpheus-runtime-join".into())
            .spawn(move || {
                if supervisor_wait.recv().is_err() {
                    return;
                }
                let joined = match worker.lock() {
                    Ok(mut slot) => match slot.take() {
                        Some(worker) => {
                            drop(slot);
                            worker.join().unwrap_or_else(|payload| {
                                retain_panic(payload);
                                Err(Failure::FinalizerPanicked)
                            })
                        }
                        None => Err(Failure::Ownership),
                    },
                    Err(_) => Err(Failure::Ownership),
                };
                if let Ok(mut result) = result.lock() {
                    *result = Some(joined);
                }
            });
        match supervisor {
            Ok(supervisor) => {
                owner.supervisor = Some(supervisor);
                (
                    owner,
                    Some(FinalizerStart {
                        worker: worker_start,
                        supervisor: supervisor_start,
                    }),
                )
            }
            Err(_) => {
                // Dropping the unopened gates lets the worker return without
                // taking the Runtime. Its actual handle stays in owner.worker.
                owner.start_failure = Some(Failure::FinalizerStart);
                (owner, None)
            }
        }
    }

    fn observer(&self) -> FinalizerObservation {
        FinalizerObservation {
            result: self.result.clone(),
            start_failure: self.start_failure,
        }
    }
}

impl Drop for Finalizer {
    fn drop(&mut self) {
        // A native AppKit exit need not unwind the outer Rust frame. Retain
        // the small supervisor tail/handle; do not claim that it was joined.
        if let Some(supervisor) = self.supervisor.take() {
            mem::forget(supervisor);
        }
        if self.observer().read().ok().flatten().is_none() {
            mem::forget(self.worker.clone());
            mem::forget(self.result.clone());
        }
    }
}

fn retain_panic(payload: Box<dyn Any + Send>) {
    // Never display a payload or run its arbitrary destructor in a native
    // callback. This is a sticky failure, not a live-task/join receipt.
    mem::forget(payload);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        time::{Duration, Instant},
    };

    struct TlsReceipt(Arc<AtomicBool>);
    impl Drop for TlsReceipt {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    thread_local! {
        static TLS_RECEIPT: RefCell<Option<TlsReceipt>> = const { RefCell::new(None) };
    }

    // A bounded observation, not a production timeout or a join substitute.
    fn settle<R: Send + 'static>(owner: &Coordinator<R>) -> bool {
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            let _ = owner.poll();
            let finished = owner
                .state
                .borrow()
                .as_ref()
                .is_some_and(|state| state.phase == Phase::Finished);
            if finished {
                return true;
            }
            thread::sleep(Duration::from_millis(1));
        }
        false
    }

    #[test]
    fn clean_waits_for_actual_cleanup_and_runtime_thread_tls_and_runs_once() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let entered = Arc::new(AtomicBool::new(false));
        let tls_released = Arc::new(AtomicBool::new(false));
        let conclusions = Arc::new(AtomicUsize::new(0));
        let (release, wait) = tokio::sync::oneshot::channel();
        let actual_entered = entered.clone();
        let actual_tls = tls_released.clone();
        let actual_conclusions = conclusions.clone();
        let observation_entered = Arc::new(AtomicBool::new(false));
        let actual_observation_entered = observation_entered.clone();
        let (observation_release, observation_wait) = tokio::sync::oneshot::channel();
        let owner = Coordinator::new(
            runtime,
            move || async move {
                TLS_RECEIPT.with(|slot| *slot.borrow_mut() = Some(TlsReceipt(actual_tls)));
                actual_entered.store(true, Ordering::Release);
                let _ = wait.await;
                17
            },
            move |result| {
                actual_conclusions.fetch_add(1, Ordering::AcqRel);
                if result == Ok(17) {
                    Completion::observe(Ok(()), async move {
                        actual_observation_entered.store(true, Ordering::Release);
                        let _ = observation_wait.await;
                        OwnerOutcome::finalize(Ok(()))
                    })
                } else {
                    Completion::retain(Failure::Cleanup(ShutdownStep::Draft))
                }
            },
        );
        let started = owner.start(None);
        let repeated = owner.start(None);
        let until = Instant::now() + Duration::from_secs(5);
        while !entered.load(Ordering::Acquire) && Instant::now() < until {
            let _ = owner.poll();
            thread::sleep(Duration::from_millis(1));
        }
        let entered_before = entered.load(Ordering::Acquire);
        let before = owner.poll();
        let tls_before = tls_released.load(Ordering::Acquire);
        // Always release the real future before evaluating any observation.
        let _ = release.send(());
        let until = Instant::now() + Duration::from_secs(5);
        while !observation_entered.load(Ordering::Acquire) && Instant::now() < until {
            let _ = owner.poll();
            thread::sleep(Duration::from_millis(1));
        }
        let pending_observation = owner.poll();
        let observation_was_entered = observation_entered.load(Ordering::Acquire);
        let runtime_retained = owner.runtime.lock().unwrap().is_some();
        let tls_during_observation = tls_released.load(Ordering::Acquire);
        let conclusions_during_observation = conclusions.load(Ordering::Acquire);
        // Also release the same-owner observation before any assertion.
        let _ = observation_release.send(());
        let settled = settle(&owner);
        let result = if settled {
            owner.finish_after_event_loop()
        } else {
            Err(Failure::Ownership)
        };
        assert!(started && repeated && entered_before);
        assert_eq!(before, Observation::Pending);
        assert!(!tls_before);
        assert!(observation_was_entered && runtime_retained);
        assert_eq!(pending_observation, Observation::Pending);
        assert!(!tls_during_observation);
        assert_eq!(conclusions_during_observation, 1);
        assert!(settled);
        assert_eq!(result, Ok(()));
        assert!(tls_released.load(Ordering::Acquire));
        assert_eq!(conclusions.load(Ordering::Acquire), 1);
    }

    #[test]
    fn failed_product_outcome_still_finalizes_runtime_and_cannot_be_clean() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let owner = Coordinator::new(
            runtime,
            || async { () },
            |_| Completion::finalize(Err(Failure::Cleanup(ShutdownStep::Draft))),
        );
        let started = owner.start(None);
        let settled = settle(&owner);
        let result = if settled {
            owner.finish_after_event_loop()
        } else {
            Err(Failure::Ownership)
        };
        assert!(started && settled);
        assert_eq!(result, Err(Failure::Cleanup(ShutdownStep::Draft)));
        assert_eq!(
            owner.poll(),
            Observation::Failed(Failure::Cleanup(ShutdownStep::Draft))
        );
    }

    #[test]
    fn task_panic_cannot_finalize_runtime_via_a_permissive_conclusion() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let owner: Coordinator<()> = Coordinator::new(
            runtime,
            || async { panic!("synthetic cleanup panic") },
            |_| Completion::finalize(Ok(())),
        );
        let started = owner.start(None);
        let until = Instant::now() + Duration::from_secs(5);
        while owner.poll() == Observation::Pending && Instant::now() < until {
            thread::sleep(Duration::from_millis(1));
        }
        let observation = owner.poll();
        let finalized = owner.runtime_finalized();
        let actual_runtime = owner.runtime.lock().unwrap().take();
        let was_retained = actual_runtime.is_some();
        drop(actual_runtime);
        assert!(started && was_retained && !finalized);
        assert_eq!(observation, Observation::Failed(Failure::CleanupPanicked));
    }

    #[test]
    fn unexpected_loop_return_never_becomes_a_native_clean_receipt() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (release, wait) = tokio::sync::oneshot::channel();
        let owner = Coordinator::new(
            runtime,
            move || async move {
                let _ = wait.await;
            },
            |_| Completion::finalize(Ok(())),
        );
        let started = owner.start(None);
        owner.after_event_loop_return();
        let _ = release.send(());
        let settled = settle(&owner);
        let result = if settled {
            owner.finish_after_event_loop()
        } else {
            Err(Failure::Ownership)
        };
        assert!(started && settled);
        assert_eq!(result, Err(Failure::EventLoopEnded));
    }

    #[test]
    fn unproved_owner_keeps_the_actual_runtime_without_inventing_a_pending_task() {
        struct ActualResources {
            identity: Arc<()>,
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let conclusions = Arc::new(AtomicUsize::new(0));
        let actual_conclusions = conclusions.clone();
        let identity = Arc::new(());
        let resources = ActualResources {
            identity: identity.clone(),
        };
        let owner = Coordinator::new(
            runtime,
            move || async move { resources },
            move |result| {
                actual_conclusions.fetch_add(1, Ordering::AcqRel);
                match result {
                    Ok(resources) => Completion::retain_resources(Failure::Ownership, resources),
                    Err(_) => Completion::retain(Failure::Ownership),
                }
            },
        );
        let started = owner.start(None);
        let until = Instant::now() + Duration::from_secs(5);
        while conclusions.load(Ordering::Acquire) == 0 && Instant::now() < until {
            let _ = owner.poll();
            thread::sleep(Duration::from_millis(1));
        }
        let observation = owner.poll();
        let finalized = owner.runtime_finalized();
        let actual_resources = owner.retained_resources::<ActualResources>();
        let exact_resources = actual_resources
            .as_ref()
            .is_some_and(|resources| Arc::ptr_eq(&identity, &resources.identity));
        let no_fabricated_task = owner.state.borrow().as_ref().is_some_and(|state| {
            state.phase == Phase::Retained
                && state.task.is_none()
                && state.observation_task.is_none()
                && state.finalizer.is_none()
        });
        // This test owns the otherwise retained executor and explicitly closes
        // it before assertions. That is not a clean Coordinator receipt.
        let actual_runtime = owner.runtime.lock().unwrap().take();
        let was_retained = actual_runtime.is_some();
        drop(actual_runtime);
        assert!(started && was_retained && no_fabricated_task && exact_resources);
        assert!(!finalized);
        assert_eq!(observation, Observation::Failed(Failure::Ownership));
        assert_eq!(conclusions.load(Ordering::Acquire), 1);
    }
}
