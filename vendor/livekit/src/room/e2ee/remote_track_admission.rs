// Morpheus private E2EE track admission state.
// SPDX-License-Identifier: Apache-2.0
// See LICENSE and MORPHEUS_PATCH.md.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use super::EncryptionType;

#[derive(Default)]
struct AdmissionState {
    closed: bool,
    tracks: HashMap<(String, String), Arc<()>>,
}

/// Serializes receiver preparation and export against close. Instance tokens
/// retain no client, track, key, cryptor or native handle.
#[derive(Default)]
pub(super) struct RemoteTrackAdmissions {
    state: Mutex<AdmissionState>,
}

impl RemoteTrackAdmissions {
    fn lock(&self) -> MutexGuard<'_, AdmissionState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                // The original callback panic remains a failure. Cleanup may
                // still proceed, but no later callback may export media.
                let mut state = poisoned.into_inner();
                state.closed = true;
                state.tracks.clear();
                state
            }
        }
    }

    pub(super) fn admit(
        &self,
        required: bool,
        encryption: EncryptionType,
        key: (String, String),
        instance: Arc<()>,
        prepare: impl FnOnce(bool),
        export: impl FnOnce(),
    ) -> bool {
        let mut state = self.lock();
        if state.closed || (required && encryption == EncryptionType::None) {
            return false;
        }
        prepare(required);
        if required {
            state.tracks.insert(key, instance);
        }
        // Keep the gate through dispatch, so close cannot fall between a
        // successful install and the Room exporting that receiver.
        export();
        true
    }

    pub(super) fn retire(
        &self,
        required: bool,
        key: (String, String),
        instance: &Arc<()>,
        remove: impl FnOnce(),
        export: impl FnOnce(),
    ) -> bool {
        let mut state = self.lock();
        if state.closed {
            return false;
        }
        let ours = state.tracks.get(&key).is_some_and(|current| Arc::ptr_eq(current, instance));
        if required && !ours {
            return false;
        }
        state.tracks.remove(&key);
        remove();
        export();
        true
    }

    pub(super) fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        state.tracks.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    fn key() -> (String, String) {
        ("peer".into(), "track".into())
    }

    #[test]
    fn configured_e2ee_refuses_clear_tracks_before_prepare_or_export() {
        for encryption in [EncryptionType::None, EncryptionType::Gcm, EncryptionType::Custom] {
            let admissions = RemoteTrackAdmissions::default();
            let instance = Arc::new(());
            let prepared = Cell::new(false);
            let exported = Cell::new(false);
            let admitted = admissions.admit(
                true, encryption, key(), instance.clone(),
                |required| {
                    assert!(required);
                    prepared.set(true);
                },
                || exported.set(true),
            );
            let encrypted = encryption != EncryptionType::None;
            assert_eq!(admitted, encrypted);
            assert_eq!(prepared.get(), encrypted);
            assert_eq!(exported.get(), encrypted);
            assert_eq!(admissions.retire(true, key(), &instance, || {}, || {}), encrypted);
            assert!(!admissions.retire(true, key(), &instance, || {}, || {}));
        }
    }

    #[test]
    fn non_e2ee_rooms_keep_preparation_and_export_without_cryptor_setup() {
        for encryption in [EncryptionType::None, EncryptionType::Gcm, EncryptionType::Custom] {
            let admissions = RemoteTrackAdmissions::default();
            let instance = Arc::new(());
            let prepared = Cell::new(false);
            let exported = Cell::new(false);
            assert!(admissions.admit(
                false, encryption, key(), instance.clone(),
                |required| {
                    assert!(!required);
                    prepared.set(true);
                },
                || exported.set(true),
            ));
            assert!(prepared.get());
            assert!(exported.get());
            assert!(admissions.retire(false, key(), &instance, || {}, || {}));
        }
    }

    #[test]
    fn a_failed_install_panics_without_export_and_closes_later_admission() {
        let admissions = RemoteTrackAdmissions::default();
        let exported = Cell::new(false);
        let result = catch_unwind(AssertUnwindSafe(|| {
            admissions.admit(
                true, EncryptionType::Gcm, key(), Arc::new(()),
                |_| panic!("receiver installation failed"),
                || exported.set(true),
            );
        }));
        assert!(result.is_err(), "the original panic remains a failure");
        assert!(!exported.get());
        admissions.close();
        assert!(!admissions.admit(
            true, EncryptionType::Gcm, key(), Arc::new(()),
            |_| panic!("closed preparation"), || panic!("closed export"),
        ));
        assert!(admissions.lock().tracks.is_empty());
    }

    #[test]
    fn overlapping_same_sid_instances_cannot_retire_each_others_receiver() {
        let admissions = RemoteTrackAdmissions::default();
        let older = Arc::new(());
        let newer = Arc::new(());
        let refused = Arc::new(());
        let removed = Cell::new(0);
        let exported = Cell::new(0);
        assert!(admissions.admit(true, EncryptionType::Gcm, key(), older.clone(), |_| {}, || {}));
        assert!(!admissions.admit(
            true, EncryptionType::None, key(), refused.clone(), |_| panic!("clear setup"),
            || panic!("clear export"),
        ));
        assert!(!admissions.retire(
            true, key(), &refused, || removed.set(removed.get() + 1),
            || exported.set(exported.get() + 1),
        ));
        assert!(admissions.admit(true, EncryptionType::Gcm, key(), newer.clone(), |_| {}, || {}));
        assert!(!admissions.retire(
            true, key(), &older, || removed.set(removed.get() + 1),
            || exported.set(exported.get() + 1),
        ));
        assert_eq!((removed.get(), exported.get()), (0, 0));
        assert!(admissions.retire(
            true, key(), &newer, || removed.set(removed.get() + 1),
            || exported.set(exported.get() + 1),
        ));
        assert_eq!((removed.get(), exported.get()), (1, 1));
        assert!(!admissions.retire(true, key(), &newer, || {}, || {}));
    }

    #[test]
    fn close_fences_entered_export_and_refuses_detached_late_callbacks() {
        let admissions = Arc::new(RemoteTrackAdmissions::default());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (exported_tx, exported_rx) = mpsc::channel();
        let (closing_tx, closing_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();
        let install_gate = admissions.clone();
        let installer = std::thread::spawn(move || {
            assert!(install_gate.admit(
                true, EncryptionType::Gcm, key(), Arc::new(()),
                |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                },
                || exported_tx.send(()).unwrap(),
            ));
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let close_gate = admissions.clone();
        let closer = std::thread::spawn(move || {
            closing_tx.send(()).unwrap();
            close_gate.close();
            closed_tx.send(()).unwrap();
        });
        closing_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(closed_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        release_tx.send(()).unwrap();
        installer.join().unwrap();
        closer.join().unwrap();
        exported_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        closed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!admissions.admit(
            true, EncryptionType::Gcm, key(), Arc::new(()),
            |_| panic!("late setup"), || panic!("late export"),
        ));
        assert!(!admissions.retire(
            true, key(), &Arc::new(()), || panic!("late removal"), || panic!("late export"),
        ));
        assert!(admissions.lock().tracks.is_empty());
    }
}
