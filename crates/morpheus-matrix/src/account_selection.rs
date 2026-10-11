//! Revocable ownership of asynchronous work by one account selection.

use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};

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
}

impl AccountSelection {
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub fn is_current(&self) -> bool {
        self.current.load(Ordering::Acquire)
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
        }
        *selected = account_id.map(|account_id| AccountSelection {
            account_id: account_id.to_owned(),
            current: Arc::new(AtomicBool::new(true)),
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
