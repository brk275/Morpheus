//! Encrypted room-key files tied to the exact selected loaded client.
//! CPU work is synchronous inside the caller's retained native worker. Admitted
//! SDK operations are awaited to completion, never select-dropped on UI cancel.

use crate::{
    AccountManager, AccountSelection, LoadedAccount, State, account_access, selected_loaded_for,
};
use matrix_sdk::encryption::room_key_file::{
    BoundedKeyExportError, RoomKeyFileError as SdkError, prepare_room_key_import,
};
use std::sync::Arc;

/// Fixed public text only; no keys, passphrase, file paths or SDK error details.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RoomKeyFileError {
    #[error(
        "The selected account changed. No new key-store import or file publication was admitted."
    )]
    AccountChanged,
    #[error(
        "The file operation was cancelled or a security step changed. No new store import or file publication was admitted."
    )]
    NotAdmitted,
    #[error("The encrypted room-key file or its passphrase is invalid.")]
    InvalidFile,
    #[error("The room-key file exceeds a supported resource limit. No partial backup was made.")]
    Limit,
    #[error("Enter the key-file passphrase; new export passphrases require 12–1024 UTF-8 bytes.")]
    Passphrase,
    #[error(
        "Room keys are unavailable for this account. Try again after encryption setup settles."
    )]
    Unavailable,
    #[error("The encrypted account keys could not be read. No export was produced.")]
    ReadFailed,
    #[error(
        "Room-key import completion could not be confirmed. Some keys may have been imported; it was not retried."
    )]
    ImportUncertain,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn publication_rechecks_after_state_wait_and_never_revives_same_account_id() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let root = std::env::temp_dir()
                .join(format!("morpheus-key-file-scope-{}", uuid::Uuid::new_v4()));
            let manager = AccountManager::open(&root).unwrap();
            let id = "synthetic-account".to_owned();
            let client = matrix_sdk::Client::builder()
                .homeserver_url("http://127.0.0.1:9")
                .build()
                .await
                .unwrap();
            let loaded = Arc::new(LoadedAccount::new(
                client,
                id.clone(),
                String::new(),
                manager._access_lock.clone(),
            ));
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts.push(crate::AccountInfo {
                    id: id.clone(),
                    user_id: "@fixture:localhost".into(),
                    device_id: "FIXTURE".into(),
                    homeserver: "http://127.0.0.1:9".into(),
                });
                state.active_account_id = Some(id.clone());
                state.loaded.insert(id.clone(), loaded.clone());
            }
            manager.selections.replace(Some(&id));
            let selection = manager.account_selection(&id).unwrap();
            let scope = manager.room_key_file_scope_for(&selection).await;
            let calls = AtomicUsize::new(0);
            let result = if let Ok(scope) = scope {
                let held = manager.state.lock().await;
                let admitted = || true;
                let mut commit =
                    Box::pin(manager.commit_room_key_file_for(&scope, &admitted, || {
                        calls.fetch_add(1, Ordering::Relaxed)
                    }));
                let waited = futures_util::poll!(commit.as_mut()).is_pending();
                manager.selections.replace(None);
                manager.selections.replace(Some(&id));
                drop(held);
                let result = commit.await;
                Some((waited, result))
            } else {
                None
            };
            manager.stop().await;
            loaded.stop_owned_tasks().await;
            drop(loaded);
            drop(manager);
            let cleaned = std::fs::remove_dir_all(&root).is_ok();
            assert_eq!(result, Some((true, Err(RoomKeyFileError::AccountChanged))));
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert!(cleaned);
        });
    }

    #[test]
    fn publication_requires_same_loaded_client_and_preserves_native_outcome() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let root = std::env::temp_dir()
                .join(format!("morpheus-key-file-client-{}", uuid::Uuid::new_v4()));
            let manager = AccountManager::open(&root).unwrap();
            let id = "synthetic-account".to_owned();
            let first = matrix_sdk::Client::builder()
                .homeserver_url("http://127.0.0.1:9")
                .build()
                .await
                .unwrap();
            let second = matrix_sdk::Client::builder()
                .homeserver_url("http://127.0.0.1:9")
                .build()
                .await
                .unwrap();
            let first = Arc::new(LoadedAccount::new(
                first,
                id.clone(),
                String::new(),
                manager._access_lock.clone(),
            ));
            let second = Arc::new(LoadedAccount::new(
                second,
                id.clone(),
                String::new(),
                manager._access_lock.clone(),
            ));
            {
                let mut state = manager.state.lock().await;
                state.registry.accounts.push(crate::AccountInfo {
                    id: id.clone(),
                    user_id: "@fixture:localhost".into(),
                    device_id: "FIXTURE".into(),
                    homeserver: "http://127.0.0.1:9".into(),
                });
                state.active_account_id = Some(id.clone());
                state.loaded.insert(id.clone(), first.clone());
            }
            manager.selections.replace(Some(&id));
            let selection = manager.account_selection(&id).unwrap();
            let scope = manager.room_key_file_scope_for(&selection).await;
            let calls = AtomicUsize::new(0);
            let observed = if let Ok(scope) = scope {
                let accepted = manager
                    .commit_room_key_file_for(&scope, &|| true, || {
                        calls.fetch_add(1, Ordering::Relaxed);
                        "native-publication-unknown"
                    })
                    .await;
                let denied = manager
                    .commit_room_key_file_for(&scope, &|| false, || {
                        calls.fetch_add(1, Ordering::Relaxed);
                        "not-called"
                    })
                    .await;
                manager
                    .state
                    .lock()
                    .await
                    .loaded
                    .insert(id.clone(), second.clone());
                let replaced = manager
                    .commit_room_key_file_for(&scope, &|| true, || {
                        calls.fetch_add(1, Ordering::Relaxed);
                        "not-called"
                    })
                    .await;
                Some((accepted, denied, replaced))
            } else {
                None
            };
            manager.stop().await;
            first.stop_owned_tasks().await;
            second.stop_owned_tasks().await;
            drop(first);
            drop(second);
            drop(manager);
            let cleaned = std::fs::remove_dir_all(&root).is_ok();
            assert_eq!(
                observed,
                Some((
                    Ok("native-publication-unknown"),
                    Err(RoomKeyFileError::NotAdmitted),
                    Err(RoomKeyFileError::AccountChanged)
                ))
            );
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert!(cleaned);
        });
    }
}
impl From<SdkError> for RoomKeyFileError {
    fn from(error: SdkError) -> Self {
        match error {
            SdkError::File(BoundedKeyExportError::InvalidFile) => Self::InvalidFile,
            SdkError::File(BoundedKeyExportError::Limit) => Self::Limit,
            SdkError::File(BoundedKeyExportError::Passphrase) => Self::Passphrase,
            SdkError::StoreUnavailable => Self::Unavailable,
            SdkError::StoreRead => Self::ReadFailed,
            SdkError::ImportUncertain => Self::ImportUncertain,
        }
    }
}

/// One captured loaded client, not an account-ID lookup that can revive later.
/// No credentials/key material can be formatted or serialized from this type.
pub struct RoomKeyFileScope {
    selection: AccountSelection,
    loaded: Arc<LoadedAccount>,
}
/// Ciphertext only. The complete snapshot is never silently truncated.
pub struct EncryptedRoomKeyFile {
    bytes: String,
    keys: usize,
}
impl EncryptedRoomKeyFile {
    pub fn bytes(&self) -> &[u8] {
        self.bytes.as_bytes()
    }
    pub fn key_count(&self) -> usize {
        self.keys
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoomKeyImportCounts {
    pub imported: usize,
    pub sdk_valid: usize,
    pub file_records: usize,
}

impl AccountManager {
    /// Read the exact already-loaded account only; never opens a vault/store or
    /// restores a different client. Call before the native chooser starts.
    pub async fn room_key_file_scope_for(
        &self,
        selection: &AccountSelection,
    ) -> Result<RoomKeyFileScope, RoomKeyFileError> {
        let state = self.state.lock().await;
        let current = self
            .account_selection(selection.account_id())
            .ok_or(RoomKeyFileError::AccountChanged)?;
        if !selection.is_current() || !selection.same_epoch(&current) {
            return Err(RoomKeyFileError::AccountChanged);
        }
        account_access::ensure_eligible(&state, selection.account_id())
            .map_err(|_| RoomKeyFileError::AccountChanged)?;
        let loaded = selected_loaded_for(&state, selection.account_id())
            .map_err(|_| RoomKeyFileError::AccountChanged)?;
        Ok(RoomKeyFileScope {
            selection: selection.clone(),
            loaded: loaded.clone(),
        })
    }

    fn check_room_key_scope(
        &self,
        state: &State,
        scope: &RoomKeyFileScope,
    ) -> Result<(), RoomKeyFileError> {
        let current = self
            .account_selection(scope.selection.account_id())
            .ok_or(RoomKeyFileError::AccountChanged)?;
        if !scope.selection.is_current() || !scope.selection.same_epoch(&current) {
            return Err(RoomKeyFileError::AccountChanged);
        }
        account_access::ensure_eligible(state, scope.selection.account_id())
            .map_err(|_| RoomKeyFileError::AccountChanged)?;
        let loaded = selected_loaded_for(state, scope.selection.account_id())
            .map_err(|_| RoomKeyFileError::AccountChanged)?;
        if !Arc::ptr_eq(loaded, &scope.loaded) {
            return Err(RoomKeyFileError::AccountChanged);
        }
        Ok(())
    }

    /// Run this entire future on the caller's retained CPU/native worker.
    /// `admitted` is the ORIGINAL work lease/security generation, not a newly
    /// captured UI state. No cancellation/select is allowed after store handoff.
    pub async fn import_room_key_file_for(
        &self,
        scope: &RoomKeyFileScope,
        bytes: &[u8],
        passphrase: &str,
        admitted: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<RoomKeyImportCounts, RoomKeyFileError> {
        if !scope.selection.is_current() || !admitted() {
            return Err(RoomKeyFileError::NotAdmitted);
        }
        let prepared = prepare_room_key_import(bytes, passphrase)?;
        // Lock order is manager state -> SDK machine, not the reverse. No
        // account switch/removal can replace this loaded client while retained.
        let state = self.state.lock().await;
        self.check_room_key_scope(&state, scope)?;
        let encryption = scope.loaded.client.encryption();
        let target = encryption.room_key_import_store().await?;
        // ALL preparation awaits are complete. The next await enters existing
        // SDK store import directly and both actual guards remain retained.
        self.check_room_key_scope(&state, scope)?;
        if !admitted() {
            return Err(RoomKeyFileError::NotAdmitted);
        }
        let counts = target.import(prepared).await?;
        Ok(RoomKeyImportCounts {
            imported: counts.imported_count,
            sdk_valid: counts.total_count,
            file_records: counts.file_key_count,
        })
    }

    /// Read ALL captured account keys and encrypt on the retained native worker.
    /// Store-read and CPU completion are awaited despite cancelled presentation;
    /// no file is written here. Final publication uses the separate admission.
    pub async fn encrypt_room_key_file_for(
        &self,
        scope: &RoomKeyFileScope,
        passphrase: &str,
        admitted: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<EncryptedRoomKeyFile, RoomKeyFileError> {
        {
            let state = self.state.lock().await;
            self.check_room_key_scope(&state, scope)?;
            if !admitted() {
                return Err(RoomKeyFileError::NotAdmitted);
            }
        }
        let snapshot = scope
            .loaded
            .client
            .encryption()
            .prepare_room_key_export()
            .await?;
        let keys = snapshot.key_count();
        if !scope.selection.is_current() || !admitted() {
            return Err(RoomKeyFileError::NotAdmitted);
        }
        let bytes = snapshot.encrypt(passphrase)?;
        Ok(EncryptedRoomKeyFile { bytes, keys })
    }

    /// Await the exact account scope, then synchronously invoke one no-clobber
    /// publication. No await or new authority capture occurs between the final
    /// check and this handoff. The native caller reports its actual commit state
    /// (including uncertainty); this API does not overwrite, retry or roll back.
    pub async fn commit_room_key_file_for<T>(
        &self,
        scope: &RoomKeyFileScope,
        admitted: &(dyn Fn() -> bool + Send + Sync),
        commit: impl FnOnce() -> T,
    ) -> Result<T, RoomKeyFileError> {
        let state = self.state.lock().await;
        self.check_room_key_scope(&state, scope)?;
        if !admitted() {
            return Err(RoomKeyFileError::NotAdmitted);
        }
        Ok(commit())
    }
}
