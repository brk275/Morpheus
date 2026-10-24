//! Explicit ownership for application-lifetime config polling. The receiver
//! APIs remain available for compatibility; ordinary desktop callers retain this
//! owner and observe its actual thread join during shutdown.

use std::sync::mpsc::{self, Receiver, RecvError, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Clone)]
pub struct WatchStop(Sender<()>);

impl WatchStop {
    /// Wake the worker even if the config and theme have not changed.
    pub fn stop(&self) {
        let _ = self.0.send(());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchJoinError {
    WorkerPanicked,
}

impl std::fmt::Display for WatchJoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The configuration watcher did not finish normally.")
    }
}

impl std::error::Error for WatchJoinError {}

pub struct OwnedWatch<T> {
    updates: Receiver<T>,
    stop: WatchStop,
    worker: Option<JoinHandle<()>>,
}

impl<T> OwnedWatch<T> {
    pub fn recv(&self) -> Result<T, RecvError> {
        self.updates.recv()
    }

    pub fn stop_handle(&self) -> WatchStop {
        self.stop.clone()
    }

    /// Blocking, off the UI thread. Success includes the worker's actual TLS
    /// destruction, rather than only its body-end flag or a receiver close.
    pub fn finish(mut self) -> Result<(), WatchJoinError> {
        self.stop.stop();
        let worker = self
            .worker
            .take()
            .expect("owned watcher retains its worker");
        worker.join().map_err(|payload| {
            // A panic payload is neither configuration diagnostics nor a safe
            // value to format or run arbitrary destructors for during cleanup.
            std::mem::forget(payload);
            WatchJoinError::WorkerPanicked
        })
    }
}

impl<T> Drop for OwnedWatch<T> {
    fn drop(&mut self) {
        self.stop.stop();
        // Dropping the owner is cancellation, never a successful join receipt.
        // Preserve an unobserved handle instead of silently detaching it.
        if let Some(worker) = self.worker.take() {
            std::mem::forget(worker);
        }
    }
}

pub(super) fn spawn<T: Send + 'static>(
    interval: Duration,
    mut sample: impl FnMut() -> Option<T> + Send + 'static,
) -> std::io::Result<OwnedWatch<T>> {
    if interval.is_zero() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "The configuration watch interval must be positive.",
        ));
    }
    // One pending notification is enough for desktop callers, which reread the
    // current file on the UI thread. Coalescing never blocks cancellation.
    let (updates, receiver) = mpsc::sync_channel(1);
    let (stop, stopped) = mpsc::channel();
    let worker = thread::Builder::new()
        .name("morpheus-config-watch".into())
        .spawn(move || {
            loop {
                match stopped.recv_timeout(interval) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {}
                }
                if let Some(value) = sample() {
                    match updates.try_send(value) {
                        Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                        Err(mpsc::TrySendError::Disconnected(_)) => break,
                    }
                }
            }
        })?;
    Ok(OwnedWatch {
        updates: receiver,
        stop: WatchStop(stop),
        worker: Some(worker),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct ExitFlag(Arc<AtomicBool>);
    impl Drop for ExitFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    thread_local! {
        static ON_EXIT: RefCell<Option<ExitFlag>> = const { RefCell::new(None) };
    }

    #[test]
    fn stopping_an_unchanged_watch_wakes_the_receiver_before_its_next_poll() {
        let watch = spawn(Duration::from_secs(3600), || Some(())).unwrap();
        watch.stop_handle().stop();
        let observed = watch.updates.recv_timeout(Duration::from_secs(2));
        let finished = watch.finish();
        assert!(matches!(observed, Err(RecvTimeoutError::Disconnected)));
        finished.unwrap();
    }

    #[test]
    fn a_finished_watch_observes_the_actual_workers_tls_destructor() {
        let exited = Arc::new(AtomicBool::new(false));
        let on_exit = exited.clone();
        let watch = spawn(Duration::from_millis(1), move || {
            ON_EXIT.with(|slot| {
                let mut slot = slot.borrow_mut();
                if slot.is_none() {
                    *slot = Some(ExitFlag(on_exit.clone()));
                }
            });
            Some(())
        })
        .unwrap();
        let observed = watch.updates.recv_timeout(Duration::from_secs(2));
        let before_join = exited.load(Ordering::Acquire);
        let finished = watch.finish();
        observed.unwrap();
        finished.unwrap();
        assert!(!before_join);
        assert!(exited.load(Ordering::Acquire));
    }

    #[test]
    fn a_panicked_watch_is_a_fixed_failed_join_not_a_clean_receiver_close() {
        let watch = spawn::<()>(Duration::from_millis(1), || {
            panic!("synthetic watcher failure")
        })
        .unwrap();
        let observed = watch.updates.recv_timeout(Duration::from_secs(2));
        let finished = watch.finish();
        assert!(matches!(observed, Err(RecvTimeoutError::Disconnected)));
        assert_eq!(finished, Err(WatchJoinError::WorkerPanicked));
    }
}
