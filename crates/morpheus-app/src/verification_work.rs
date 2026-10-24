//! Actual app verification-future ownership shared by own-device and peer UI.
//!
//! A UI result, cancelled view or empty protocol inbox is not a task receipt.
//! Every admitted future stays in this registry until its actual JoinHandle
//! returns. Account removal/logout pause the exact account before draining;
//! terminal shutdown closes all admission before the same drain. No join handle
//! is taken into a timeout future or a detached waiter. This does not prove
//! completion of work an SDK has independently delegated to a server or backend.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

use morpheus_matrix::{AccountSelection, VerificationCancellation};
use tokio::{runtime::Handle, task::JoinHandle};

// One own-device and one peer view, including their action/observer overlap.
// These are refusal ceilings, never proof of completion or permission to evict.
const MAX_TASKS: usize = 32;
const MAX_ACCOUNT_SCOPES: usize = 64;
const DRAIN_LIMIT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerificationDrainError {
    Pending,
    Failed,
}
impl std::fmt::Display for VerificationDrainError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Pending => "Verification work is still stopping. Wait, then try again.",
            Self::Failed => "Verification task completion could not be confirmed. Restart Morpheus before removing or logging out this account.",
        })
    }
}
impl std::error::Error for VerificationDrainError {}

/// Terminal application shutdown only. Account pause/removal retains its
/// stricter existing Failed result. Worker proves all registered tasks joined;
/// Unproved also includes native cleanup whose completion was not established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerificationShutdownError {
    Pending,
    Worker,
    Unproved,
}
impl std::fmt::Display for VerificationShutdownError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Pending => "Verification work is still stopping.",
            Self::Worker => "Verification tasks finished with an error.",
            Self::Unproved => "Verification cleanup could not be confirmed.",
        })
    }
}
impl std::error::Error for VerificationShutdownError {}

#[derive(Default)]
pub(crate) struct VerificationWork {
    owner: Arc<Owner>,
}

/// Captured on the UI thread with the original account selection and view
/// cancellation. A clean pause/resume creates a new account capability; this
/// exact lease never revives, even if the same local account ID is selected.
#[derive(Clone)]
pub(crate) struct VerificationWorkLease {
    work: ScopeLease,
    selection: AccountSelection,
}
impl VerificationWorkLease {
    pub(crate) fn is_current(&self) -> bool {
        self.selection.is_current() && self.work.is_current()
    }

    /// Record unproved native effects/cleanup AFTER that operation's real
    /// worker and supervisor joins. This is sticky failure, not a join receipt;
    /// the enclosing actual task handle remains registered until it returns.
    pub(crate) fn fail_native_completion(&self) {
        self.work.fail_native_completion();
    }
}

#[derive(Default)]
struct Owner {
    state: Mutex<State>,
}
#[derive(Default)]
struct State {
    closed: bool,
    ownership_failed: bool,
    worker_failed: bool,
    native_failed: bool,
    accounts: HashMap<String, Account>,
    tasks: Vec<Task>,
}
struct Account {
    epoch: Arc<Epoch>,
    pause: Option<Pause>,
    failed: bool,
}
struct Epoch {
    account: String,
    current: AtomicBool,
}
struct Pause {
    identity: Arc<()>,
    held: bool,
}
struct Task {
    epoch: Arc<Epoch>,
    cancellation: VerificationCancellation,
    handle: JoinHandle<()>,
}
#[derive(Clone)]
struct ScopeLease {
    owner: Arc<Owner>,
    epoch: Arc<Epoch>,
    cancellation: VerificationCancellation,
}

impl Owner {
    fn with<R>(&self, action: impl FnOnce(&mut State) -> R) -> R {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(error) => {
                let mut state = error.into_inner();
                state.ownership_failed = true;
                state.close();
                state
            }
        };
        action(&mut state)
    }

    fn capture(
        self: &Arc<Self>,
        account: &str,
        cancellation: &VerificationCancellation,
    ) -> Option<ScopeLease> {
        self.with(|state| {
            state.poll();
            if state.closed || state.ownership_failed || cancellation.is_cancelled() {
                return None;
            }
            state.prune();
            if !state.accounts.contains_key(account) {
                if state.accounts.len() >= MAX_ACCOUNT_SCOPES {
                    return None;
                }
                state
                    .accounts
                    .insert(account.to_owned(), Account::new(account));
            }
            let entry = state.accounts.get(account)?;
            if entry.pause.is_some() || entry.failed {
                return None;
            }
            Some(ScopeLease {
                owner: self.clone(),
                epoch: entry.epoch.clone(),
                cancellation: cancellation.clone(),
            })
        })
    }

    fn register(
        &self,
        lease: &ScopeLease,
        selected: bool,
        runtime: &Handle,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> bool {
        self.with(|state| {
            state.poll();
            if !selected || !state.current(lease) || state.tasks.len() >= MAX_TASKS {
                return false;
            }
            // Reserve before spawning, so registration needs no allocation after
            // Tokio creates the actual task. Spawn does not synchronously poll it.
            if state.tasks.try_reserve(1).is_err() {
                return false;
            }
            let handle = runtime.spawn(future);
            state.tasks.push(Task {
                epoch: lease.epoch.clone(),
                cancellation: lease.cancellation.clone(),
                handle,
            });
            true
        })
    }
}
impl Account {
    fn new(account: &str) -> Self {
        Self {
            epoch: Arc::new(Epoch {
                account: account.to_owned(),
                current: AtomicBool::new(true),
            }),
            pause: None,
            failed: false,
        }
    }
}
impl ScopeLease {
    fn is_current(&self) -> bool {
        self.owner.with(|state| state.current(self))
    }

    fn fail_native_completion(&self) {
        self.epoch.current.store(false, Ordering::Release);
        self.cancellation.cancel();
        self.owner.with(|state| {
            state.native_failed = true;
            let Some(account) = state
                .accounts
                .get_mut(&self.epoch.account)
                .filter(|account| Arc::ptr_eq(&account.epoch, &self.epoch))
            else {
                // Never bless or selectively mark a replacement account from
                // a stale/missing epoch. Unknown ownership closes all admission.
                state.ownership_failed = true;
                state.close();
                return;
            };
            account.failed = true;
            account.epoch.current.store(false, Ordering::Release);
            self.cancellation.cancel();
            for task in &state.tasks {
                if Arc::ptr_eq(&task.epoch, &self.epoch) {
                    task.cancellation.cancel();
                }
            }
        });
    }
}
impl State {
    fn current(&self, lease: &ScopeLease) -> bool {
        !self.closed
            && !self.ownership_failed
            && !lease.cancellation.is_cancelled()
            && lease.epoch.current.load(Ordering::Acquire)
            && self
                .accounts
                .get(&lease.epoch.account)
                .is_some_and(|account| {
                    account.pause.is_none()
                        && !account.failed
                        && Arc::ptr_eq(&account.epoch, &lease.epoch)
                })
    }

    fn prune(&mut self) {
        self.accounts.retain(|_, account| {
            account.pause.is_some() || account.failed || Arc::strong_count(&account.epoch) != 1
        });
    }

    fn close(&mut self) {
        self.closed = true;
        for account in self.accounts.values() {
            account.epoch.current.store(false, Ordering::Release);
        }
        for task in &self.tasks {
            task.cancellation.cancel();
        }
    }

    fn poll(&mut self) {
        let mut context = Context::from_waker(Waker::noop());
        let mut index = 0;
        while index < self.tasks.len() {
            match Pin::new(&mut self.tasks[index].handle).poll(&mut context) {
                Poll::Ready(result) => {
                    let task = self.tasks.swap_remove(index);
                    if let Err(error) = result {
                        // Do not run an arbitrary panic-payload destructor while
                        // the registry mutex is held (including final Drop).
                        if error.is_panic() {
                            std::mem::forget(error.into_panic());
                        }
                        self.worker_failed = true;
                        // No abort is requested by this owner. Any JoinError,
                        // including runtime cancellation, is a sticky failure.
                        if let Some(account) = self.accounts.get_mut(&task.epoch.account) {
                            account.failed = true;
                            account.epoch.current.store(false, Ordering::Release);
                        } else {
                            self.ownership_failed = true;
                        }
                        for pending in &self.tasks {
                            if pending.epoch.account == task.epoch.account {
                                pending.cancellation.cancel();
                            }
                        }
                    }
                    // The sole handle is dropped only AFTER its poll receipt.
                    drop(task);
                }
                Poll::Pending => index += 1,
            }
        }
    }

    fn receipt(&mut self, account: Option<&str>) -> Option<Result<(), VerificationDrainError>> {
        self.poll();
        if self
            .tasks
            .iter()
            .any(|task| account.is_none_or(|id| task.epoch.account == id))
        {
            return None;
        }
        let failed = self.ownership_failed
            || self.accounts.iter().any(|(id, entry)| {
                account.is_none_or(|wanted| wanted == id.as_str()) && entry.failed
            });
        Some(if failed {
            Err(VerificationDrainError::Failed)
        } else {
            Ok(())
        })
    }
}
impl Drop for State {
    fn drop(&mut self) {
        self.close();
        self.poll();
        if !self.tasks.is_empty() {
            // An abnormal last-owner path cannot invent a receipt or detach by
            // dropping the sole handles. Retain the actual pending owners.
            // Normal account/application teardown uses the checked drain below.
            std::mem::forget(std::mem::take(&mut self.tasks));
        }
    }
}

impl VerificationWork {
    pub(crate) fn lease(
        &self,
        selection: &AccountSelection,
        cancellation: &VerificationCancellation,
    ) -> Option<VerificationWorkLease> {
        if !selection.is_current() {
            return None;
        }
        let work = self.owner.capture(selection.account_id(), cancellation)?;
        Some(VerificationWorkLease {
            work,
            selection: selection.clone(),
        })
    }

    pub(crate) fn spawn(
        &self,
        runtime: &Handle,
        lease: &VerificationWorkLease,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> bool {
        Arc::ptr_eq(&self.owner, &lease.work.owner)
            && self
                .owner
                .register(&lease.work, lease.selection.is_current(), runtime, future)
    }

    pub(crate) fn close(&self) {
        self.owner.with(State::close);
    }

    pub(crate) async fn drain(&self) -> Result<(), VerificationDrainError> {
        wait_for_receipt(
            || {
                self.owner.with(|state| {
                    if !state.closed {
                        Some(Err(VerificationDrainError::Failed))
                    } else {
                        state.receipt(None)
                    }
                })
            },
            DRAIN_LIMIT,
        )
        .await
    }

    /// Reobserve the same whole-session registry after synchronous close. A
    /// bounded observation expires with Pending while every original handle
    /// remains reachable; callers may await this same owner again before stop.
    pub(crate) async fn finish_shutdown(&self) -> Result<(), VerificationShutdownError> {
        let deadline = tokio::time::Instant::now() + DRAIN_LIMIT;
        loop {
            let result = self.owner.with(|state| {
                if !state.closed {
                    return Some(Err(VerificationShutdownError::Unproved));
                }
                state.poll();
                if !state.tasks.is_empty() {
                    return None;
                }
                Some(if state.ownership_failed || state.native_failed {
                    Err(VerificationShutdownError::Unproved)
                } else if state.worker_failed {
                    Err(VerificationShutdownError::Worker)
                } else {
                    Ok(())
                })
            });
            if let Some(result) = result {
                return result;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(VerificationShutdownError::Pending);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// The caller holds account_transition and its existing security lease.
    /// Logout MUST check its captured selection before this ID-scoped pause.
    /// A failed drain leaves the account paused; retry can observe later joins.
    pub(crate) fn pause(
        &self,
        account: &str,
    ) -> Result<VerificationWorkPause, VerificationDrainError> {
        self.owner.with(|state| {
            state.poll();
            if state.closed || state.ownership_failed {
                return Err(VerificationDrainError::Failed);
            }
            state.prune();
            if !state.accounts.contains_key(account) {
                if state.accounts.len() >= MAX_ACCOUNT_SCOPES {
                    return Err(VerificationDrainError::Pending);
                }
                state
                    .accounts
                    .insert(account.to_owned(), Account::new(account));
            }
            let entry = state
                .accounts
                .get_mut(account)
                .ok_or(VerificationDrainError::Failed)?;
            if entry.failed {
                return Err(VerificationDrainError::Failed);
            }
            if entry.pause.as_ref().is_some_and(|pause| pause.held) {
                return Err(VerificationDrainError::Pending);
            }
            entry.epoch.current.store(false, Ordering::Release);
            let identity = entry
                .pause
                .get_or_insert_with(|| Pause {
                    identity: Arc::new(()),
                    held: false,
                })
                .identity
                .clone();
            entry
                .pause
                .as_mut()
                .ok_or(VerificationDrainError::Failed)?
                .held = true;
            for task in &state.tasks {
                if task.epoch.account == account {
                    task.cancellation.cancel();
                }
            }
            Ok(VerificationWorkPause {
                owner: self.owner.clone(),
                account: account.to_owned(),
                identity,
                clean: false,
            })
        })
    }
}

/// Held through the destructive operation. Only a successful actual drain may
/// resume this exact pause; guard cancellation/timeouts never reopen admission.
pub(crate) struct VerificationWorkPause {
    owner: Arc<Owner>,
    account: String,
    identity: Arc<()>,
    clean: bool,
}
impl VerificationWorkPause {
    pub(crate) async fn drain(&mut self) -> Result<(), VerificationDrainError> {
        self.drain_with_limit(DRAIN_LIMIT).await
    }

    async fn drain_with_limit(&mut self, limit: Duration) -> Result<(), VerificationDrainError> {
        let result = wait_for_receipt(
            || {
                self.owner.with(|state| {
                    if state.closed || state.ownership_failed {
                        return Some(Err(VerificationDrainError::Failed));
                    }
                    if !state
                        .accounts
                        .get(&self.account)
                        .and_then(|entry| entry.pause.as_ref())
                        .is_some_and(|pause| {
                            pause.held && Arc::ptr_eq(&pause.identity, &self.identity)
                        })
                    {
                        return Some(Err(VerificationDrainError::Failed));
                    }
                    state.receipt(Some(&self.account))
                })
            },
            limit,
        )
        .await;
        self.clean = result.is_ok();
        result
    }
}
impl Drop for VerificationWorkPause {
    fn drop(&mut self) {
        self.owner.with(|state| {
            state.poll();
            let pending = state
                .tasks
                .iter()
                .any(|task| task.epoch.account == self.account);
            let Some(entry) = state.accounts.get_mut(&self.account) else {
                return;
            };
            let Some(pause) = &mut entry.pause else {
                return;
            };
            if !Arc::ptr_eq(&pause.identity, &self.identity) {
                return;
            }
            pause.held = false;
            if self.clean && !pending && !entry.failed && !state.closed && !state.ownership_failed {
                // Never flip the retired epoch back to true (queued callback ABA).
                entry.epoch = Account::new(&self.account).epoch;
                entry.pause = None;
            }
        });
    }
}

async fn wait_for_receipt(
    mut poll: impl FnMut() -> Option<Result<(), VerificationDrainError>>,
    limit: Duration,
) -> Result<(), VerificationDrainError> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if let Some(result) = poll() {
            return result;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(VerificationDrainError::Pending);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    async fn cleanup(work: &VerificationWork) -> Result<(), VerificationDrainError> {
        work.close();
        let result = wait_for_receipt(
            || work.owner.with(|state| state.receipt(None)),
            Duration::from_secs(3),
        )
        .await;
        if result == Err(VerificationDrainError::Pending) {
            // Do not drop the last registry owner on an unproved test cleanup.
            std::mem::forget(work.owner.clone());
        }
        result
    }

    #[test]
    fn account_pause_retains_actual_tasks_and_never_revives_old_ui_leases() {
        let runtime = runtime();
        runtime.block_on(async {
            let work = VerificationWork::default();
            let cancellation = VerificationCancellation::new();
            let lease = work.owner.capture("a", &cancellation).unwrap();
            let (release, released) = tokio::sync::oneshot::channel();
            let (started, beginning) = tokio::sync::oneshot::channel();
            let dropped = Arc::new(AtomicBool::new(false));
            struct Held(Arc<AtomicBool>);
            impl Drop for Held {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            let drop_receipt = dropped.clone();
            let admitted = work
                .owner
                .register(&lease, true, runtime.handle(), async move {
                    let _held = Held(drop_receipt);
                    let _ = started.send(());
                    // Deliberately hold the actual task after logical cancellation.
                    let _ = released.await;
                });
            let entered = tokio::time::timeout(Duration::from_secs(3), beginning).await;
            let pause = work.pause("a");
            let observations = if let Ok(mut pause) = pause {
                let invalidated = !lease.is_current() && cancellation.is_cancelled();
                let refused = !work
                    .owner
                    .register(&lease, true, runtime.handle(), async {});
                let pending = pause.drain_with_limit(Duration::ZERO).await;
                let still_owned = work.owner.with(|state| state.tasks.len());
                drop(pause); // Pending must not reopen the account.
                let refused_capture = work
                    .owner
                    .capture("a", &VerificationCancellation::new())
                    .is_none();
                Some((invalidated, refused, pending, still_owned, refused_capture))
            } else {
                None
            };
            // Every path releases the actual task BEFORE any assertion.
            let _ = release.send(());
            let resumed = match work.pause("a") {
                Ok(mut pause) => {
                    let result = pause.drain_with_limit(Duration::from_secs(3)).await;
                    drop(pause);
                    result
                }
                Err(error) => Err(error),
            };
            let old_stays_revoked = !lease.is_current();
            let fresh = work.owner.capture("a", &VerificationCancellation::new());
            let fresh_current = fresh.as_ref().is_some_and(ScopeLease::is_current);
            let final_cleanup = cleanup(&work).await;
            assert!(admitted);
            assert!(matches!(entered, Ok(Ok(()))));
            assert_eq!(
                observations,
                Some((true, true, Err(VerificationDrainError::Pending), 1, true))
            );
            assert_eq!(resumed, Ok(()));
            assert!(old_stays_revoked && fresh_current);
            assert!(dropped.load(Ordering::Acquire));
            assert_eq!(final_cleanup, Ok(()));
        });
    }

    #[test]
    fn terminal_close_cancels_but_waits_for_the_real_join_and_refuses_new_work() {
        let runtime = runtime();
        runtime.block_on(async {
            let work = VerificationWork::default();
            let cancellation = VerificationCancellation::new();
            let lease = work.owner.capture("a", &cancellation).unwrap();
            let (release, released) = tokio::sync::oneshot::channel();
            let admitted = work
                .owner
                .register(&lease, true, runtime.handle(), async move {
                    let _ = released.await;
                });
            work.close();
            let pending = wait_for_receipt(
                || work.owner.with(|state| state.receipt(None)),
                Duration::ZERO,
            )
            .await;
            let owned = work.owner.with(|state| state.tasks.len());
            let refused = !work
                .owner
                .register(&lease, true, runtime.handle(), async {});
            let refused_capture = work
                .owner
                .capture("b", &VerificationCancellation::new())
                .is_none();
            let _ = release.send(());
            let final_cleanup = cleanup(&work).await;
            assert!(admitted && cancellation.is_cancelled());
            assert_eq!(pending, Err(VerificationDrainError::Pending));
            assert_eq!(owned, 1);
            assert!(refused && refused_capture && !lease.is_current());
            assert_eq!(final_cleanup, Ok(()));
        });
    }

    #[test]
    fn task_panic_is_joined_and_sticky_not_a_successful_account_drain() {
        let runtime = runtime();
        runtime.block_on(async {
            let work = VerificationWork::default();
            let lease = work
                .owner
                .capture("a", &VerificationCancellation::new())
                .unwrap();
            let payload_dropped = Arc::new(AtomicBool::new(false));
            struct Payload(Arc<AtomicBool>);
            impl Drop for Payload {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            let payload = Payload(payload_dropped.clone());
            let admitted = work
                .owner
                .register(&lease, true, runtime.handle(), async move {
                    std::panic::panic_any(payload);
                });
            let result = wait_for_receipt(
                || work.owner.with(|state| state.receipt(Some("a"))),
                Duration::from_secs(3),
            )
            .await;
            let refused = !work
                .owner
                .register(&lease, true, runtime.handle(), async {});
            let pause_failed = matches!(work.pause("a"), Err(VerificationDrainError::Failed));
            let final_cleanup = cleanup(&work).await;
            let shutdown = work.finish_shutdown().await;
            let repeated = work.finish_shutdown().await;
            assert_eq!(shutdown, Err(VerificationShutdownError::Worker));
            assert_eq!(repeated, shutdown);
            assert!(!payload_dropped.load(Ordering::Acquire));
            assert!(admitted && refused && pause_failed);
            assert_eq!(result, Err(VerificationDrainError::Failed));
            assert_eq!(final_cleanup, Err(VerificationDrainError::Failed));
            assert_eq!(work.owner.with(|state| state.tasks.len()), 0);
        });
    }

    #[test]
    fn account_pause_isolated_and_cancelled_or_unselected_work_never_starts() {
        let runtime = runtime();
        runtime.block_on(async {
            let work = VerificationWork::default();
            let a_cancel = VerificationCancellation::new();
            let b_cancel = VerificationCancellation::new();
            let a = work.owner.capture("a", &a_cancel).unwrap();
            let b = work.owner.capture("b", &b_cancel).unwrap();
            let polled = Arc::new(AtomicUsize::new(0));
            let probe = polled.clone();
            let unselected = !work
                .owner
                .register(&a, false, runtime.handle(), async move {
                    probe.fetch_add(1, Ordering::Relaxed);
                });
            let pause = work.pause("a");
            let other_current = b.is_current() && !b_cancel.is_cancelled();
            let probe = polled.clone();
            let paused = !work.owner.register(&a, true, runtime.handle(), async move {
                probe.fetch_add(1, Ordering::Relaxed);
            });
            let concurrent_pause_refused =
                matches!(work.pause("a"), Err(VerificationDrainError::Pending));
            let a_result = match pause {
                Ok(mut pause) => {
                    let result = pause.drain_with_limit(Duration::from_secs(3)).await;
                    drop(pause);
                    result
                }
                Err(error) => Err(error),
            };
            b_cancel.cancel();
            let probe = polled.clone();
            let cancelled = !work.owner.register(&b, true, runtime.handle(), async move {
                probe.fetch_add(1, Ordering::Relaxed);
            });
            let final_cleanup = cleanup(&work).await;
            assert!(unselected && paused && concurrent_pause_refused && other_current && cancelled);
            assert_eq!(a_result, Ok(()));
            assert!(!a.is_current());
            assert_eq!(polled.load(Ordering::Relaxed), 0);
            assert_eq!(final_cleanup, Ok(()));
        });
    }

    #[test]
    fn native_failure_waits_for_actual_task_then_stays_failed_without_fake_pending() {
        let runtime = runtime();
        runtime.block_on(async {
            let work = VerificationWork::default();
            let cancellation = VerificationCancellation::new();
            let lease = work.owner.capture("a", &cancellation).unwrap();
            let other = work
                .owner
                .capture("b", &VerificationCancellation::new())
                .unwrap();
            let (release, released) = tokio::sync::oneshot::channel();
            let registered = work
                .owner
                .register(&lease, true, runtime.handle(), async move {
                    let _ = released.await;
                });
            // This hook does not claim either native or Tokio completion.
            lease.fail_native_completion();
            let pending = work.owner.with(|state| state.receipt(Some("a")));
            let owned = work.owner.with(|state| state.tasks.len());
            let other_current = other.is_current();
            let _ = release.send(());
            let receipt = wait_for_receipt(
                || work.owner.with(|state| state.receipt(Some("a"))),
                Duration::from_secs(3),
            )
            .await;
            let final_cleanup = cleanup(&work).await;
            let shutdown = work.finish_shutdown().await;
            assert_eq!(shutdown, Err(VerificationShutdownError::Unproved));
            assert!(registered && cancellation.is_cancelled() && !lease.is_current());
            assert!(other_current);
            assert_eq!(pending, None);
            assert_eq!(owned, 1);
            assert_eq!(receipt, Err(VerificationDrainError::Failed));
            assert_eq!(final_cleanup, Err(VerificationDrainError::Failed));
            assert_eq!(work.owner.with(|state| state.tasks.len()), 0);
        });
    }

    #[test]
    fn native_failure_from_stale_epoch_closes_instead_of_blessing_replacement() {
        let work = VerificationWork::default();
        let old = work
            .owner
            .capture("a", &VerificationCancellation::new())
            .unwrap();
        work.owner.with(|state| {
            state.accounts.insert("a".to_owned(), Account::new("a"));
        });
        let replacement = work
            .owner
            .capture("a", &VerificationCancellation::new())
            .unwrap();
        old.fail_native_completion();
        let observed = work
            .owner
            .with(|state| (state.closed, state.ownership_failed, state.receipt(None)));
        assert!(!old.is_current() && !replacement.is_current());
        assert_eq!(
            observed,
            (true, true, Some(Err(VerificationDrainError::Failed)))
        );
    }
}
