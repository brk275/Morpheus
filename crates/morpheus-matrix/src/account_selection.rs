//! Revocable ownership of asynchronous work by one account selection.

use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;

/// A token for one selection of a local account. Returning to the same account
/// creates a different token; previously queued work never becomes current
/// again. This token contains no credentials and is never persisted.
///
/// Check immediately before presenting asynchronous results. This is a
/// cancellation signal, not a lock: work already handed to an OS service or a
/// server cannot be recalled by revoking the token.
#[derive(Clone, Debug)]
pub struct AccountSelection {
    account_id: String,
    current: Arc<AtomicBool>,
    revocation: Arc<Notify>,
}

impl AccountSelection {
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// Internal capability identity; matching account IDs alone do not bind
    /// work to the same manager and account-selection epoch.
    pub(crate) fn same_epoch(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.current, &other.current)
    }

    pub fn is_current(&self) -> bool {
        self.current.load(Ordering::Acquire)
    }

    /// Wait until this selection is permanently revoked. Every clone is woken
    /// immediately on revocation, before account switching waits for sync to
    /// stop. Waiting after revocation also completes immediately. Cancelling
    /// this future does not consume another waiter's notification.
    pub async fn revoked(&self) {
        loop {
            let notified = self.revocation.notified();
            tokio::pin!(notified);
            // Register before checking the flag: notify_waiters does not keep
            // a permit for a future waiter, so checking first could lose a wake.
            notified.as_mut().enable();
            if !self.is_current() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Default)]
pub(crate) struct Selections(RwLock<Option<AccountSelection>>);

impl Selections {
    pub fn for_account(&self, account_id: &str) -> Option<AccountSelection> {
        self.0
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .filter(|selection| selection.account_id == account_id)
            .cloned()
    }

    pub fn replace(&self, account_id: Option<&str>) {
        let mut selected = self.0.write().unwrap_or_else(|error| error.into_inner());
        if let Some(previous) = selected.take() {
            previous.current.store(false, Ordering::Release);
            previous.revocation.notify_waiters();
        }
        *selected = account_id.map(|account_id| AccountSelection {
            account_id: account_id.to_owned(),
            current: Arc::new(AtomicBool::new(true)),
            revocation: Arc::new(Notify::new()),
        });
    }
}

impl Drop for Selections {
    fn drop(&mut self) {
        self.replace(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[test]
    fn revocation_before_wait_completes_without_a_saved_permit() {
        runtime().block_on(async {
            let selections = Selections::default();
            selections.replace(Some("alice"));
            let selection = selections.for_account("alice").unwrap();
            selections.replace(None);
            for _ in 0..2 {
                tokio::time::timeout(Duration::from_secs(1), selection.revoked())
                    .await
                    .expect("already revoked selection did not complete");
            }
        });
    }

    #[test]
    fn replacement_wakes_every_registered_clone() {
        runtime().block_on(async {
            let selections = Selections::default();
            selections.replace(Some("alice"));
            let copies: Vec<_> = (0..8)
                .map(|_| selections.for_account("alice").unwrap())
                .collect();
            let mut waiters: Vec<_> = copies
                .iter()
                .map(|selection| Box::pin(selection.revoked()))
                .collect();
            for waiter in &mut waiters {
                assert!(futures_util::poll!(waiter.as_mut()).is_pending());
            }
            selections.replace(Some("bob"));
            for waiter in waiters {
                tokio::time::timeout(Duration::from_secs(1), waiter)
                    .await
                    .expect("a registered clone missed revocation");
            }
        });
    }

    #[test]
    fn dropped_waiter_cannot_consume_another_waiters_revocation() {
        runtime().block_on(async {
            let selections = Selections::default();
            selections.replace(Some("alice"));
            let selection = selections.for_account("alice").unwrap();
            let mut cancelled = Box::pin(selection.revoked());
            assert!(futures_util::poll!(cancelled.as_mut()).is_pending());
            drop(cancelled);
            let mut retained = Box::pin(selection.revoked());
            assert!(futures_util::poll!(retained.as_mut()).is_pending());
            selections.replace(None);
            tokio::time::timeout(Duration::from_secs(1), retained)
                .await
                .expect("a cancelled waiter consumed revocation");
        });
    }

    #[test]
    fn returning_to_the_same_account_keeps_new_waiters_pending_until_owner_drop() {
        runtime().block_on(async {
            let selections = Selections::default();
            selections.replace(Some("alice"));
            let original = selections.for_account("alice").unwrap();
            let mut original_wait = Box::pin(original.revoked());
            assert!(futures_util::poll!(original_wait.as_mut()).is_pending());
            selections.replace(Some("bob"));
            let intermediate = selections.for_account("bob").unwrap();
            let mut intermediate_wait = Box::pin(intermediate.revoked());
            assert!(futures_util::poll!(intermediate_wait.as_mut()).is_pending());
            selections.replace(Some("alice"));
            let current = selections.for_account("alice").unwrap();
            let mut current_wait = Box::pin(current.revoked());
            assert!(futures_util::poll!(current_wait.as_mut()).is_pending());
            tokio::time::timeout(Duration::from_secs(1), original_wait)
                .await
                .expect("original selection was revived");
            tokio::time::timeout(Duration::from_secs(1), intermediate_wait)
                .await
                .expect("intermediate selection remained active");
            assert!(futures_util::poll!(current_wait.as_mut()).is_pending());
            drop(selections);
            tokio::time::timeout(Duration::from_secs(1), current_wait)
                .await
                .expect("owner drop did not wake the current selection");
        });
    }

    #[test]
    fn coalesced_account_switch_never_revives_queued_work() {
        let selections = Selections::default();
        assert!(selections.for_account("alice").is_none());
        selections.replace(Some("alice"));
        let original = selections.for_account("alice").unwrap();
        let queued = original.clone();
        selections.replace(Some("bob"));
        let bob = selections.for_account("bob").unwrap();
        selections.replace(Some("alice"));
        assert!(!original.is_current());
        assert!(!queued.is_current());
        assert!(!bob.is_current());
        assert!(selections.for_account("bob").is_none());
        assert!(selections.for_account("alice").unwrap().is_current());
    }

    #[test]
    fn shutdown_or_owner_drop_revokes_all_clones() {
        let selections = Selections::default();
        selections.replace(Some("alice"));
        let before_shutdown = selections.for_account("alice").unwrap();
        selections.replace(None);
        assert!(!before_shutdown.is_current());
        assert!(selections.for_account("alice").is_none());
        selections.replace(Some("alice"));
        let after_restart = selections.for_account("alice").unwrap();
        drop(selections);
        assert!(!after_restart.is_current());
    }

    #[test]
    fn capability_identity_rejects_another_manager_or_selection_with_the_same_account_id() {
        let first = Selections::default();
        let second = Selections::default();
        first.replace(Some("casey-local"));
        second.replace(Some("casey-local"));
        let original = first.for_account("casey-local").unwrap();
        let foreign = second.for_account("casey-local").unwrap();
        assert!(original.is_current() && foreign.is_current());
        assert!(original.same_epoch(&original.clone()));
        assert!(!original.same_epoch(&foreign));
        first.replace(Some("jules-local"));
        first.replace(Some("casey-local"));
        assert!(!original.same_epoch(&first.for_account("casey-local").unwrap()));
        assert!(!original.is_current());
    }

    #[test]
    fn same_account_reauthentication_invalidates_old_selection() {
        let selections = Selections::default();
        selections.replace(Some("alice"));
        let previous = selections.for_account("alice").unwrap();
        selections.replace(Some("alice"));
        assert!(!previous.is_current());
        assert_eq!(
            selections.for_account("alice").unwrap().account_id(),
            "alice"
        );
    }
}
