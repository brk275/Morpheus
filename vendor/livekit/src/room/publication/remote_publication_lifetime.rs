// Morpheus private remote publication lifetime gate.
// SPDX-License-Identifier: Apache-2.0
// See LICENSE and MORPHEUS_PATCH.md.

use std::sync::{Mutex, MutexGuard};

/// One publication instance only; clones share it through RemoteInner.
/// No track, key, client, SID, native handle or task is retained here.
#[derive(Default)]
pub(super) struct RemotePublicationLifetime {
    retired: Mutex<bool>,
}

impl RemotePublicationLifetime {
    fn lock(&self) -> MutexGuard<'_, bool> {
        match self.retired.lock() {
            Ok(retired) => retired,
            Err(poisoned) => {
                // The original callback panic stays a failure. A partially
                // entered export must never permit a later receiver to enter.
                let mut retired = poisoned.into_inner();
                *retired = true;
                retired
            }
        }
    }

    pub(super) fn admit(&self, export: impl FnOnce()) -> bool {
        let retired = self.lock();
        if *retired {
            return false;
        }
        // Keep the gate through receiver preparation and callback dispatch.
        export();
        true
    }

    pub(super) fn clear_receiver(&self, cleanup: impl FnOnce()) {
        let _retired = self.lock();
        // Temporary unsubscribe may clear even after an unwind, but must
        // never reopen a retired publication or retire a still-live one.
        cleanup();
    }

    pub(super) fn retire(&self, cleanup: impl FnOnce()) {
        let mut retired = self.lock();
        *retired = true;
        // Cleanup remains allowed after an unwind or repeated retirement.
        cleanup();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, TryLockError};
    use std::time::Duration;

    use super::*;

    #[test]
    fn retired_publication_refuses_late_clones_without_retiring_a_fresh_instance() {
        let publication = Arc::new(RemotePublicationLifetime::default());
        let delayed = publication.clone();
        let exports = AtomicUsize::new(0);
        for _ in 0..2 {
            assert!(publication.admit(|| {
                exports.fetch_add(1, Ordering::SeqCst);
            }));
            publication.clear_receiver(|| {});
        }
        publication.retire(|| {});
        assert!(!delayed.admit(|| {
            exports.fetch_add(1, Ordering::SeqCst);
        }));
        let replacement = RemotePublicationLifetime::default();
        assert!(replacement.admit(|| {
            exports.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(exports.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn entered_export_completes_before_retirement_and_no_late_export_can_enter() {
        let publication = Arc::new(RemotePublicationLifetime::default());
        let exported = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker_publication = publication.clone();
        let worker_exported = exported.clone();
        let worker = std::thread::spawn(move || {
            worker_publication.admit(|| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                worker_exported.store(true, Ordering::SeqCst);
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(publication.retired.try_lock(), Err(TryLockError::WouldBlock)));
        let retiring_publication = publication.clone();
        let retiring_exported = exported.clone();
        let (retiring_tx, retiring_rx) = mpsc::channel();
        let retiring = std::thread::spawn(move || {
            retiring_tx.send(()).unwrap();
            retiring_publication.retire(|| {
                assert!(retiring_exported.load(Ordering::SeqCst));
            });
        });
        retiring_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        release_tx.send(()).unwrap();
        assert!(worker.join().unwrap());
        retiring.join().unwrap();
        assert!(!publication.admit(|| panic!("late export must not enter")));
    }

    #[test]
    fn callback_unwind_keeps_failure_closes_admission_and_allows_cleanup() {
        let publication = RemotePublicationLifetime::default();
        let failure = std::panic::catch_unwind(|| {
            publication.admit(|| panic!("fixture callback failure"));
        });
        assert!(failure.is_err());
        assert!(!publication.admit(|| panic!("poisoned export must not enter")));
        let cleanups = AtomicUsize::new(0);
        publication.clear_receiver(|| {
            cleanups.fetch_add(1, Ordering::SeqCst);
        });
        assert!(!publication.admit(|| panic!("cleanup must not reopen admission")));
        for _ in 0..2 {
            publication.retire(|| {
                cleanups.fetch_add(1, Ordering::SeqCst);
            });
        }
        assert_eq!(cleanups.load(Ordering::SeqCst), 3);
    }
}
