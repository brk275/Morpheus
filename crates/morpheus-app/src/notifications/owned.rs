//! The notification surfaces' fixed set of application tasks.
//!
//! Closing wakes idle tasks; it does not abort an entered presenter operation.
//! A receipt comes only from polling each retained actual Tokio JoinHandle.
//! This owner does not claim completion of native listeners spawned by the
//! current presenter, or of work handed to the operating system.

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

use tokio::{runtime::Handle, sync::Notify, task::JoinHandle};

const DRAIN_LIMIT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotificationDrainFailure {
    Pending,
    Worker,
    Ownership,
}

impl std::fmt::Display for NotificationDrainFailure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Pending => "Notification work is still stopping. Wait, then try again.",
            Self::Worker => "A notification task did not finish successfully.",
            Self::Ownership => "Notification task ownership could not be confirmed.",
        })
    }
}
impl std::error::Error for NotificationDrainFailure {}

/// Both observations have already run. Unproved ownership/completion takes
/// precedence over a known joined failure; the latter remains sticky on its
/// own actual owner and cannot turn into success on a later observation.
pub(crate) fn combine_app_work(
    first: Result<(), NotificationDrainFailure>,
    second: Result<(), NotificationDrainFailure>,
) -> Result<(), NotificationDrainFailure> {
    use NotificationDrainFailure::{Ownership, Pending, Worker};
    match (first, second) {
        (Err(Ownership), _) | (_, Err(Ownership)) => Err(Ownership),
        (Err(Pending), _) | (_, Err(Pending)) => Err(Pending),
        (Err(Worker), _) | (_, Err(Worker)) => Err(Worker),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// Fixed production roles, not an unbounded work queue. A role is registered
/// at most once for this owner, even after its task has returned.
#[derive(Clone, Copy)]
pub(crate) enum Role {
    Accounts,
    Delivery,
    PresenterEvents,
    ReadMarkers,
    Activations,
    PendingOpen,
    #[cfg(feature = "native-calls")]
    IncomingWatch,
    #[cfg(feature = "native-calls")]
    IncomingShow,
    #[cfg(feature = "native-calls")]
    IncomingEvents,
}

#[derive(Clone, Default)]
pub(crate) struct Close(Arc<Signal>);

#[derive(Default)]
struct Signal {
    closed: AtomicBool,
    wake: Notify,
}

impl Close {
    pub(crate) fn close(&self) {
        self.0.closed.store(true, Ordering::Release);
        self.0.wake.notify_waiters();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }

    pub(crate) async fn cancelled(&self) {
        loop {
            let notified = self.0.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_closed() {
                return;
            }
            notified.await;
        }
    }

    /// Only for cancellable reads/waits, never an entered native operation or
    /// its actual JoinHandle. Closing drops this future and its SDK owners.
    pub(crate) async fn guard<T>(&self, future: impl Future<Output = T>) -> Option<T> {
        tokio::select! {
            biased;
            _ = self.cancelled() => None,
            value = future => (!self.is_closed()).then_some(value),
        }
    }
}

/// The only two process-lifetime services. Their actual owner is reachable
/// before any task starts and remains so after a failed drain/wrapper Drop.
/// Completed tasks retain no future/SDK payload: only fixed receipt flags.
static DESKTOP: OnceLock<Arc<Owner>> = OnceLock::new();
#[cfg(feature = "native-calls")]
static INCOMING: OnceLock<Arc<Owner>> = OnceLock::new();

pub(crate) struct NotificationWork {
    owner: Arc<Owner>,
}

#[derive(Default)]
struct Owner {
    close: Close,
    state: Mutex<State>,
}

/// Re-observe the exact original production owners before Runtime teardown.
/// Each Pending handle is actually polled; neither role counts nor is_finished
/// can produce a receipt. This does not start work or reset admission. Local
/// fixture owners must be drained separately through their actual wrappers.
pub(crate) fn observe_retained_app_work() -> Result<(), NotificationDrainFailure> {
    fn observe(owner: Option<&Arc<Owner>>) -> Result<(), NotificationDrainFailure> {
        match owner {
            Some(owner) => {
                owner.close();
                match owner.poll()? {
                    true => Ok(()),
                    false => Err(NotificationDrainFailure::Pending),
                }
            }
            None => Ok(()),
        }
    }
    let desktop = observe(DESKTOP.get());
    #[cfg(feature = "native-calls")]
    let incoming = observe(INCOMING.get());
    #[cfg(not(feature = "native-calls"))]
    let incoming = Ok(());
    combine_app_work(desktop, incoming)
}

#[cfg(test)]
impl Default for NotificationWork {
    fn default() -> Self {
        Self {
            owner: Arc::new(Owner::default()),
        }
    }
}

#[derive(Default)]
struct State {
    registered: u16,
    handles: Vec<JoinHandle<()>>,
    worker_failed: bool,
    ownership_failed: bool,
}

impl NotificationWork {
    fn install(slot: &OnceLock<Arc<Owner>>) -> Self {
        let owner = Arc::new(Owner::default());
        if slot.set(owner.clone()).is_err() {
            // Never attach a new manager/UI wrapper to existing live actors.
            // This refused wrapper has no tasks and permanently closed admission.
            owner.close.close();
            owner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .ownership_failed = true;
        }
        Self { owner }
    }

    pub(crate) fn desktop() -> Self {
        #[cfg(test)]
        {
            Self::default()
        }
        #[cfg(not(test))]
        {
            Self::install(&DESKTOP)
        }
    }

    #[cfg(feature = "native-calls")]
    pub(crate) fn incoming() -> Self {
        #[cfg(test)]
        {
            Self::default()
        }
        #[cfg(not(test))]
        {
            Self::install(&INCOMING)
        }
    }

    pub(crate) fn cancellation(&self) -> Close {
        self.owner.close.clone()
    }

    pub(crate) fn close(&self) {
        self.owner.close.close();
    }

    pub(crate) fn spawn(
        &self,
        role: Role,
        runtime: &Handle,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> bool {
        self.owner.spawn(role, runtime, future)
    }

    #[cfg(test)]
    fn poll(&self) -> Result<bool, NotificationDrainFailure> {
        self.owner.poll()
    }

    pub(crate) async fn finish_app_work(&self) -> Result<(), NotificationDrainFailure> {
        self.owner.finish_app_work().await
    }
}

impl Owner {
    pub(crate) fn close(&self) {
        self.close.close();
    }

    pub(crate) fn spawn(
        &self,
        role: Role,
        runtime: &Handle,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> bool {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(error) => {
                let mut state = error.into_inner();
                state.ownership_failed = true;
                self.close();
                return false;
            }
        };
        let role = 1_u16 << role as u32;
        if self.close.is_closed() || state.registered & role != 0 {
            return false;
        }
        // The body cannot start before the sole actual handle is retained.
        let (start, started) = tokio::sync::oneshot::channel();
        let handle = runtime.spawn(async move {
            if started.await.is_ok() {
                future.await;
            }
        });
        state.registered |= role;
        state.handles.push(handle);
        if start.send(()).is_err() {
            state.worker_failed = true;
            self.close();
        }
        true
    }

    fn poll(&self) -> Result<bool, NotificationDrainFailure> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(error) => {
                let mut state = error.into_inner();
                state.ownership_failed = true;
                self.close();
                state
            }
        };
        let mut context = Context::from_waker(Waker::noop());
        let mut index = 0;
        while index < state.handles.len() {
            match Pin::new(&mut state.handles[index]).poll(&mut context) {
                Poll::Pending => index += 1,
                Poll::Ready(result) => {
                    // Defuse arbitrary panic-payload Drop before changing
                    // ownership. This handle must never be polled after Ready.
                    if let Err(error) = result {
                        if error.is_panic() {
                            std::mem::forget(error.into_panic());
                        }
                        state.worker_failed = true;
                        self.close();
                    }
                    drop(state.handles.swap_remove(index));
                }
            }
        }
        if state.ownership_failed {
            Err(NotificationDrainFailure::Ownership)
        } else if !state.handles.is_empty() {
            Ok(false)
        } else if state.worker_failed {
            Err(NotificationDrainFailure::Worker)
        } else {
            Ok(true)
        }
    }

    pub(crate) async fn finish_app_work(&self) -> Result<(), NotificationDrainFailure> {
        self.close();
        let drain = async {
            loop {
                if self.poll()? {
                    return Ok(());
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        };
        tokio::time::timeout(DRAIN_LIMIT, drain)
            .await
            .unwrap_or(Err(NotificationDrainFailure::Pending))
    }
}

impl Drop for NotificationWork {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn closing_wakes_idle_work_and_observes_the_actual_join() {
        let work = NotificationWork::default();
        let close = work.cancellation();
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (returned, receipt) = tokio::sync::oneshot::channel();
        assert!(work.spawn(Role::Accounts, &Handle::current(), async move {
            let _ = entered.send(());
            close.cancelled().await;
            let _ = returned.send(());
        }));
        entry.await.unwrap();
        let before = work.poll();
        work.close();
        let finished = work.finish_app_work().await;
        let body_returned = receipt.await;
        assert_eq!(before, Ok(false));
        assert_eq!(finished, Ok(()));
        assert!(body_returned.is_ok());
        assert!(!work.spawn(Role::Delivery, &Handle::current(), async {}));
    }

    #[tokio::test]
    async fn a_cancelled_drain_keeps_the_real_handle_for_later_observation() {
        let work = NotificationWork::default();
        let (release, held) = tokio::sync::oneshot::channel();
        assert!(work.spawn(Role::Delivery, &Handle::current(), async move {
            let _ = held.await;
        }));
        work.close();
        // Poll the production drain once, then cancel only the observation.
        let mut drain = Box::pin(work.finish_app_work());
        let first = std::future::poll_fn(|cx| Poll::Ready(drain.as_mut().poll(cx))).await;
        drop(drain);
        let retained = work.poll();
        let _ = release.send(());
        let final_receipt = work.finish_app_work().await;
        assert!(first.is_pending());
        assert_eq!(retained, Ok(false));
        assert_eq!(final_receipt, Ok(()));
    }

    #[tokio::test]
    async fn wrapper_drop_retains_the_exact_pending_owner_and_refuses_repeat_start() {
        let slot = OnceLock::new();
        let work = NotificationWork::install(&slot);
        let actual_owner = work.owner.clone();
        let (release, held) = tokio::sync::oneshot::channel();
        let admitted = work.spawn(Role::Delivery, &Handle::current(), async move {
            let _ = held.await;
        });
        let before = actual_owner.poll();
        drop(work);
        let retained = slot.get().cloned().unwrap();
        let repeat = NotificationWork::install(&slot);
        let repeat_closed = repeat.cancellation().is_closed();
        let repeated = repeat.spawn(Role::Accounts, &Handle::current(), async {});
        let same_owner = Arc::ptr_eq(&actual_owner, &retained);
        let still_pending = retained.poll();
        let _ = release.send(());
        let joined = retained.finish_app_work().await;
        let failed_repeat = repeat.finish_app_work().await;
        assert!(admitted);
        assert_eq!(before, Ok(false));
        assert!(same_owner && repeat_closed && !repeated);
        assert_eq!(still_pending, Ok(false));
        assert_eq!(joined, Ok(()));
        assert_eq!(failed_repeat, Err(NotificationDrainFailure::Ownership));
    }

    #[tokio::test]
    async fn real_local_service_constructors_do_not_share_or_poll_global_owners() {
        let first = NotificationWork::desktop();
        let second = NotificationWork::desktop();
        let distinct = !Arc::ptr_eq(&first.owner, &second.owner);
        let close = first.cancellation();
        let first_admitted = first.spawn(Role::Accounts, &Handle::current(), async move {
            close.cancelled().await;
        });
        first.close();
        let second_still_open = !second.cancellation().is_closed();
        let second_admitted = second.spawn(Role::Accounts, &Handle::current(), async {});
        let first_joined = first.finish_app_work().await;
        let second_joined = second.finish_app_work().await;
        assert!(distinct && first_admitted && second_still_open && second_admitted);
        assert_eq!(first_joined, Ok(()));
        assert_eq!(second_joined, Ok(()));
    }

    #[tokio::test]
    async fn joined_failure_never_masks_another_actual_owner_still_pending() {
        let failed = NotificationWork::default();
        let held = NotificationWork::default();
        let (release, release_wait) = tokio::sync::oneshot::channel();
        let failed_admitted = failed.spawn(Role::Accounts, &Handle::current(), async {
            panic!("fixed joined notification failure");
        });
        let held_admitted = held.spawn(Role::Delivery, &Handle::current(), async move {
            let _ = release_wait.await;
        });
        let joined_failure = failed.finish_app_work().await;
        let pending = held.poll().and_then(|done| {
            if done {
                Ok(())
            } else {
                Err(NotificationDrainFailure::Pending)
            }
        });
        let while_pending = combine_app_work(joined_failure, pending);
        let reverse_pending = combine_app_work(pending, joined_failure);
        let _ = release.send(());
        let held_joined = held.finish_app_work().await;
        let still_failed = failed.finish_app_work().await;
        let final_outcome = combine_app_work(still_failed, held_joined);
        assert!(failed_admitted && held_admitted);
        assert_eq!(while_pending, Err(NotificationDrainFailure::Pending));
        assert_eq!(reverse_pending, while_pending);
        assert_eq!(held_joined, Ok(()));
        assert_eq!(final_outcome, Err(NotificationDrainFailure::Worker));
        assert_eq!(
            combine_app_work(Err(NotificationDrainFailure::Ownership), while_pending),
            Err(NotificationDrainFailure::Ownership)
        );
    }

    #[tokio::test]
    async fn a_joined_panic_is_sticky_and_does_not_drop_its_payload() {
        struct HostileDrop;
        impl Drop for HostileDrop {
            fn drop(&mut self) {
                panic!("panic payload must not be dropped");
            }
        }
        let work = NotificationWork::default();
        assert!(
            work.spawn(Role::PresenterEvents, &Handle::current(), async {
                std::panic::panic_any(HostileDrop);
            })
        );
        let first = work.finish_app_work().await;
        let second = work.finish_app_work().await;
        assert_eq!(first, Err(NotificationDrainFailure::Worker));
        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn close_guard_drops_a_pending_read_and_never_reopens() {
        let close = Close::default();
        let (released, release) = tokio::sync::oneshot::channel();
        struct ReadOwner(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for ReadOwner {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }
        let held = ReadOwner(Some(released));
        close.close();
        let result = close
            .guard(async move {
                let _held = held;
                std::future::pending::<()>().await;
            })
            .await;
        let dropped = release.await;
        assert_eq!(result, None);
        assert!(dropped.is_ok());
        assert!(close.guard(async { 1 }).await.is_none());
    }
}
