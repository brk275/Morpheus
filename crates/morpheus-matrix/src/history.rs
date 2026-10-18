//! Account-selection ownership for read-only history requests.
//!
//! Keep the manager state lock out of homeserver waits so a stalled history
//! request cannot prevent account controls from acquiring it. Revocation drops
//! the read future; an HTTP request already handed to a server cannot be recalled.

use std::future::Future;

use super::*;

impl AccountManager {
    /// Load older items for the account selected when this future is polled.
    /// Returns true when the start of the room was reached.
    pub async fn paginate_backwards(&self, room_id: &str, count: u16) -> Result<bool> {
        let account_id = {
            let active = self.active_tx.borrow();
            active
                .as_ref()
                .map(|account| account.id.clone())
                .ok_or(MatrixCoreError::NoActiveAccount)?
        };
        self.paginate_backwards_for(&account_id, room_id, count)
            .await
    }

    /// Account-bound compatibility entry point. Desktop clicks should capture
    /// an AccountSelection synchronously and use paginate_backwards_selected,
    /// so queued work also rejects an account A -> B -> A sequence.
    pub async fn paginate_backwards_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        count: u16,
    ) -> Result<bool> {
        let selection = self
            .account_selection(expected_account_id)
            .ok_or(MatrixCoreError::SelectedAccountChanged)?;
        self.paginate_backwards_selected(&selection, room_id, count)
            .await
    }

    /// Read older history under the exact selection captured by the caller.
    /// Switching, removal and stop revoke the wait and its result, including
    /// when the same local account is selected again before work starts.
    pub async fn paginate_backwards_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        count: u16,
    ) -> Result<bool> {
        self.read_selected_history(selection, |loaded| async move {
            let timeline = loaded.timeline(room_id).await?;
            timeline
                .paginate_backwards(count)
                .await
                .map_err(|error| MatrixCoreError::Sdk(error.to_string()))
        })
        .await
    }

    fn owns_history_selection(&self, selection: &AccountSelection) -> bool {
        selection.is_current()
            && self
                .account_selection(selection.account_id())
                .is_some_and(|current| selection.same_epoch(&current))
    }

    // Private to this module: read-only operations only. A mutation already
    // submitted to an OS service or homeserver cannot be undone by cancellation.
    async fn read_selected_history<F, Wait>(
        &self,
        selection: &AccountSelection,
        request: F,
    ) -> Result<bool>
    where
        F: FnOnce(Arc<LoadedAccount>) -> Wait,
        Wait: Future<Output = Result<bool>>,
    {
        if !self.owns_history_selection(selection) {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        let loaded = {
            let state = self.state.lock().await;
            // The lock acquisition can itself wait through account transitions.
            if !self.owns_history_selection(selection) {
                return Err(MatrixCoreError::SelectedAccountChanged);
            }
            selected_loaded_for(&state, selection.account_id())?.clone()
        };
        selection
            .guard(async move {
                if !selection.is_current() {
                    return Err(MatrixCoreError::SelectedAccountChanged);
                }
                request(loaded).await
            })
            .await
            .ok_or(MatrixCoreError::SelectedAccountChanged)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    async fn fixture(account: Option<AccountInfo>) -> (PathBuf, Arc<AccountManager>, AccountInfo) {
        let directory =
            std::env::temp_dir().join(format!("morpheus-history-read-{}", Uuid::new_v4()));
        let manager = Arc::new(AccountManager::open(&directory).unwrap());
        let account = account.unwrap_or_else(|| AccountInfo {
            id: Uuid::new_v4().to_string(),
            user_id: "@casey:example.org".into(),
            device_id: "HISTORY-FIXTURE".into(),
            homeserver: "http://127.0.0.1:9".into(),
        });
        // No authenticated session, vault entry, sync or persistence task.
        let client = Client::builder()
            .homeserver_url(&account.homeserver)
            .build()
            .await
            .unwrap();
        client.send_queue().set_enabled(false).await;
        let loaded = Arc::new(LoadedAccount {
            client,
            timelines: Mutex::new(HashMap::new()),
            composed_edits: Mutex::new(HashMap::new()),
            pending_sends: Mutex::new(HashMap::new()),
            originals: Mutex::new(HashMap::new()),
            recovered: Mutex::new(Vec::new()),
            recovery_gate: Mutex::new(RecoveryGate::default()),
            recovery_ready: watch::channel(false).0,
            incoming_verification_tx: watch::channel(Vec::new()).0,
            notification_handler_registered: Mutex::new(false),
            token_task: Mutex::new(None),
        });
        {
            let mut state = manager.state.lock().await;
            state.registry.accounts.push(account.clone());
            state.registry.active_account_id = Some(account.id.clone());
            state.active_account_id = Some(account.id.clone());
            state.loaded.insert(account.id.clone(), loaded);
        }
        manager.publish_active_account(Some(account.clone()));
        (directory, manager, account)
    }

    #[test]
    fn stalled_history_does_not_lock_account_controls_and_revocation_releases_its_hold() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let lifetime = {
                let state = manager.state.lock().await;
                Arc::downgrade(state.loaded.get(&account.id).unwrap())
            };
            assert_eq!(lifetime.strong_count(), 1);
            let (started, received) = oneshot::channel();
            let worker_manager = manager.clone();
            let task = tokio::spawn(async move {
                worker_manager
                    .read_selected_history(&selection, move |loaded| async move {
                        let _held_client = loaded;
                        started.send(()).unwrap();
                        std::future::pending::<Result<bool>>().await
                    })
                    .await
            });
            tokio::time::timeout(Duration::from_secs(2), received)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(lifetime.strong_count(), 2);
            let accounts = tokio::time::timeout(Duration::from_secs(2), manager.accounts())
                .await
                .expect("history network wait must not retain the manager state lock");
            assert_eq!(accounts, [account.clone()]);
            // Publish a transition while holding the same state mutex account
            // activation/removal need. No history event will ever arrive.
            {
                let mut state = manager.state.lock().await;
                state.active_account_id = None;
                manager.publish_active_account(None);
            }
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(2), task)
                    .await
                    .unwrap()
                    .unwrap(),
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            assert_eq!(
                lifetime.strong_count(),
                1,
                "the revoked read drops its loaded-client hold"
            );
            drop(manager);
            fs::remove_dir_all(directory).unwrap();
        });
    }

    #[test]
    fn history_rejects_foreign_and_aba_selections_before_starting_a_request() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let (foreign_directory, foreign, _) = fixture(Some(account.clone())).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let called = calls.clone();
            assert!(matches!(
                foreign
                    .read_selected_history(&selection, move |_| {
                        called.fetch_add(1, Ordering::SeqCst);
                        async { Ok(true) }
                    })
                    .await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            {
                let mut state = manager.state.lock().await;
                let other = AccountInfo {
                    id: Uuid::new_v4().to_string(),
                    ..account.clone()
                };
                state.active_account_id = Some(other.id.clone());
                manager.publish_active_account(Some(other));
                state.active_account_id = Some(account.id.clone());
                manager.publish_active_account(Some(account.clone()));
            }
            assert!(matches!(
                manager
                    .paginate_backwards_selected(&selection, "!unused:example.org", 50)
                    .await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            let called = calls.clone();
            assert!(matches!(
                manager
                    .read_selected_history(&selection, move |_| {
                        called.fetch_add(1, Ordering::SeqCst);
                        async { Ok(true) }
                    })
                    .await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            drop(foreign);
            drop(manager);
            fs::remove_dir_all(foreign_directory).unwrap();
            fs::remove_dir_all(directory).unwrap();
        });
    }

    #[test]
    fn history_revalidates_selection_after_waiting_for_the_state_lock() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let called = calls.clone();
            let state = manager.state.lock().await;
            let mut wait = Box::pin(manager.read_selected_history(&selection, move |_| {
                called.fetch_add(1, Ordering::SeqCst);
                async { Ok(true) }
            }));
            let mut context = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
            assert!(wait.as_mut().poll(&mut context).is_pending());
            // A fresh selection of the same ID still invalidates the queued
            // request that is demonstrably waiting on the state lock.
            manager.publish_active_account(None);
            manager.publish_active_account(Some(account));
            drop(state);
            assert!(matches!(
                wait.await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            drop(manager);
            fs::remove_dir_all(directory).unwrap();
        });
    }

    #[test]
    fn history_keeps_current_results_and_errors_but_discards_a_revoked_completion() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let selection = manager.account_selection(&account.id).unwrap();
            assert!(
                manager
                    .read_selected_history(&selection, |_| async { Ok(true) })
                    .await
                    .unwrap()
            );
            assert!(matches!(
                manager
                    .read_selected_history(&selection, |_| async {
                        Err(MatrixCoreError::RoomNotFound("!missing:example.org".into()))
                    })
                    .await,
                Err(MatrixCoreError::RoomNotFound(_))
            ));
            let revoked = manager.clone();
            assert!(matches!(
                manager
                    .read_selected_history(&selection, move |_| async move {
                        revoked.publish_active_account(None);
                        Ok(true)
                    })
                    .await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            assert!(matches!(
                manager.paginate_backwards("!unused:example.org", 50).await,
                Err(MatrixCoreError::NoActiveAccount)
            ));
            drop(manager);
            fs::remove_dir_all(directory).unwrap();
        });
    }
}
