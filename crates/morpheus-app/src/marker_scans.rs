//! One coalescing owner for the voice-marker work left by a UI snapshot.
//! The worker never owns LiveUi while idle. Close retires pending work and
//! new read admission; its actual retained task still settles admitted reads.

use crate::{AppWindow, LiveUi};
use morpheus_matrix::AccountSelection;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::Poll;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub(crate) struct Request {
    pub(crate) selection: AccountSelection,
    pub(crate) rooms: Vec<String>,
    pub(crate) ui: slint::Weak<AppWindow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkerDrainError {
    Ownership,
    Worker,
}
impl std::fmt::Display for MarkerDrainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Voice marker work did not finish normally.")
    }
}
impl std::error::Error for MarkerDrainError {}

#[derive(Default)]
struct State {
    closed: bool,
    failed: bool,
    pending: Option<Request>,
}

#[derive(Default)]
struct Admission {
    state: Mutex<State>,
    changed: Notify,
}

impl Admission {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poison| {
            let mut state = poison.into_inner();
            state.closed = true;
            state.failed = true;
            state.pending.take();
            self.changed.notify_one();
            state
        })
    }

    fn close(&self) {
        let mut state = self.state();
        state.closed = true;
        state.pending.take();
        self.changed.notify_one();
    }
}

/// The original account epoch accompanies each read, cache commit and queued
/// publication. Returning to the same local ID never renews this authority.
#[derive(Clone)]
pub(crate) struct Authority {
    admission: Arc<Admission>,
    selection: AccountSelection,
    live: Weak<LiveUi>,
}

impl Authority {
    pub(crate) fn account_id(&self) -> &str {
        self.selection.account_id()
    }

    pub(crate) fn is_current(&self) -> bool {
        // Keep LiveUi alive outside the admission guard. Its final Drop also
        // closes MarkerScans and must never run while this mutex is held.
        let Some(live) = self.live.upgrade() else {
            return false;
        };
        let state = self.admission.state();
        !state.closed
            && self.selection.is_current()
            && live.is_live.load(std::sync::atomic::Ordering::Acquire)
            && !live.action_admission_closed()
    }

    /// Synchronous read admission/cache commit only. No guard crosses an await
    /// or a UI call. Close cannot interleave with this admitted cache mutation;
    /// selection remains a revocation signal, not a recall of entered work.
    pub(crate) fn with_current<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        // Declaration order also releases the guard before the last LiveUi
        // reference on refusal, successful return, or action unwinding.
        let live = self.live.upgrade()?;
        let state = self.admission.state();
        if state.closed || !self.selection.is_current() {
            return None;
        }
        if !live.is_live.load(std::sync::atomic::Ordering::Acquire)
            || live.action_admission_closed()
        {
            return None;
        }
        Some(action())
    }
}

#[derive(Default)]
pub(crate) struct MarkerScans {
    admission: Arc<Admission>,
    // Outside admission state: the worker captures only Admission and Weak<LiveUi>.
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl MarkerScans {
    #[cfg(test)]
    pub(crate) fn authority_for_test(
        &self,
        live: &Arc<LiveUi>,
        selection: AccountSelection,
    ) -> Authority {
        Authority {
            admission: self.admission.clone(),
            selection,
            live: Arc::downgrade(live),
        }
    }

    pub(crate) fn queue(&self, live: Weak<LiveUi>, runtime: &Handle, request: Request) {
        let mut worker = match self.worker.lock() {
            Ok(worker) => worker,
            Err(_) => {
                let mut state = self.admission.state();
                state.closed = true;
                state.failed = true;
                state.pending.take();
                self.admission.changed.notify_one();
                return;
            }
        };
        let mut state = self.admission.state();
        if state.closed || !request.selection.is_current() {
            return;
        }
        // Newest snapshot replaces at most one pending request; an admitted
        // active request is never dropped to make room for it.
        state.pending = Some(request);
        if worker.is_none() {
            let admission = self.admission.clone();
            // Tokio spawn never synchronously polls this future. Retention and
            // close admission are atomic under these two fixed owner locks.
            *worker = Some(runtime.spawn(run(admission, live)));
        }
        self.admission.changed.notify_one();
    }

    pub(crate) fn close(&self) {
        self.admission.close();
    }

    pub(crate) async fn drain(&self) -> Result<(), MarkerDrainError> {
        self.close();
        poll_fn(|cx| {
            let mut worker = match self.worker.lock() {
                Ok(worker) => worker,
                Err(_) => return Poll::Ready(Err(MarkerDrainError::Ownership)),
            };
            let Some(task) = worker.as_mut() else {
                return Poll::Ready(if self.admission.state().failed {
                    Err(MarkerDrainError::Ownership)
                } else {
                    Ok(())
                });
            };
            match Pin::new(task).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(result) => {
                    worker.take();
                    if let Err(error) = result {
                        if error.is_panic() {
                            std::mem::forget(error.into_panic());
                        }
                        self.admission.state().failed = true;
                        return Poll::Ready(Err(MarkerDrainError::Worker));
                    }
                    Poll::Ready(if self.admission.state().failed {
                        Err(MarkerDrainError::Ownership)
                    } else {
                        Ok(())
                    })
                }
            }
        })
        .await
    }
}

impl Drop for MarkerScans {
    fn drop(&mut self) {
        self.close();
        // Drop is revocation, not a join receipt. Keep any unobserved actual
        // task handle; ordinary main explicitly drains it before manager.stop.
        match self.worker.get_mut() {
            Ok(worker) => {
                if let Some(task) = worker.take() {
                    std::mem::forget(task);
                }
            }
            Err(poison) => {
                if let Some(task) = poison.into_inner().take() {
                    std::mem::forget(task);
                }
            }
        }
    }
}

async fn run(admission: Arc<Admission>, live: Weak<LiveUi>) {
    loop {
        let changed = admission.changed.notified();
        let request = {
            let mut state = admission.state();
            if state.closed {
                return;
            }
            state.pending.take()
        };
        if let Some(request) = request {
            let authority = Authority {
                admission: admission.clone(),
                selection: request.selection.clone(),
                live: live.clone(),
            };
            if !authority.is_current() {
                continue;
            }
            let Some(live) = live.upgrade() else {
                return;
            };
            live.run_marker_scan(request, authority).await;
            // This strong LiveUi reference is dropped here, before idle wait.
        } else {
            // notify_one retains a permit if close/queue precedes this poll.
            changed.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn close_wakes_the_real_idle_worker_and_drain_observes_its_return() {
        let owner = MarkerScans::default();
        *owner.worker.lock().unwrap() =
            Some(tokio::spawn(run(owner.admission.clone(), Weak::new())));
        tokio::task::yield_now().await;
        let joined = tokio::time::timeout(Duration::from_secs(1), owner.drain()).await;
        assert!(matches!(joined, Ok(Ok(()))));
        assert!(owner.worker.lock().unwrap().is_none());
        assert_eq!(Arc::strong_count(&owner.admission), 1);
    }

    #[tokio::test]
    async fn cancelling_a_drain_keeps_the_actual_admitted_task_until_it_returns() {
        struct Released(Arc<AtomicBool>);
        impl Drop for Released {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let owner = MarkerScans::default();
        let released = Arc::new(AtomicBool::new(false));
        let held = Released(released.clone());
        let (finish, until_finish) = tokio::sync::oneshot::channel::<()>();
        *owner.worker.lock().unwrap() = Some(tokio::spawn(async move {
            let _held = held;
            let _ = until_finish.await;
        }));
        let mut draining = Box::pin(owner.drain());
        let pending = futures_util::poll!(draining.as_mut()).is_pending();
        drop(draining);
        let retained = owner.worker.lock().unwrap().is_some();
        let not_released = !released.load(Ordering::Acquire);
        let _ = finish.send(());
        // Settle the same retained task before evaluating observations. A
        // dropped drain future is neither an abort nor a completion receipt.
        let joined = tokio::time::timeout(Duration::from_secs(1), owner.drain()).await;
        assert!(pending && retained && not_released);
        assert!(matches!(joined, Ok(Ok(()))));
        assert!(released.load(Ordering::Acquire));
        assert!(owner.worker.lock().unwrap().is_none());
    }
}
