//! Account and room scoped composer drafts.
//!
//! The UI cache makes room switches immediate. Persistence uses the Matrix
//! client's encrypted SQLite state store; no draft body is written to the
//! application config or to a plaintext fallback file.

use std::{collections::HashMap, sync::Mutex};

use morpheus_matrix::{AccountManager, MatrixCoreError};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Default)]
pub struct DraftStore {
    by_room: Mutex<HashMap<(String, String), CachedDraft>>,
    write_lock: AsyncMutex<()>,
}

struct CachedDraft {
    body: String,
    revision: u64,
    dirty: bool,
    pending: Option<PendingDraft>,
}

struct PendingDraft {
    transaction_id: String,
    submitted: String,
    queued_revision: u64,
}

impl DraftStore {
    pub fn dirty_keys(&self) -> Vec<(String, String)> {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .iter()
            .filter(|(_, draft)| draft.dirty)
            .map(|((account, room), _)| (account.clone(), room.clone()))
            .collect()
    }

    pub fn set(&self, account_id: &str, room_id: &str, body: &str) {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let entry = drafts
            .entry((account_id.to_owned(), room_id.to_owned()))
            .or_insert_with(|| CachedDraft {
                body: String::new(),
                revision: 0,
                dirty: false,
                pending: None,
            });
        // `set` is called only for a user edit. Even an edit that ends with
        // the same text must prevent an old failed send from restoring over it.
        entry.body = body.to_owned();
        entry.revision = entry.revision.wrapping_add(1);
        entry.dirty = true;
    }

    pub fn get(&self, account_id: &str, room_id: &str) -> String {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&(account_id.to_owned(), room_id.to_owned()))
            .map(|draft| draft.body.clone())
            .unwrap_or_default()
    }

    pub fn revision(&self, account_id: &str, room_id: &str) -> Option<u64> {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&(account_id.to_owned(), room_id.to_owned()))
            .map(|draft| draft.revision)
    }

    /// Hide the queued text from the UI while retaining its encrypted saved
    /// copy until this exact Matrix transaction reaches the server.
    pub fn mark_queued(
        &self,
        account_id: &str,
        room_id: &str,
        submitted: &str,
        expected_revision: u64,
        transaction_id: &str,
    ) -> bool {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let Some(draft) = drafts.get_mut(&(account_id.to_owned(), room_id.to_owned())) else {
            return false;
        };
        if draft.body != submitted || draft.revision != expected_revision {
            return false;
        }
        draft.body.clear();
        draft.revision = draft.revision.wrapping_add(1);
        draft.dirty = false;
        draft.pending = Some(PendingDraft {
            transaction_id: transaction_id.to_owned(),
            submitted: submitted.to_owned(),
            queued_revision: draft.revision,
        });
        true
    }

    /// Returns true only when the queued draft was still the empty composer.
    /// The caller should then persist the clear or restore the failed text.
    pub fn resolve_queued(
        &self,
        account_id: &str,
        room_id: &str,
        transaction_id: &str,
        sent: bool,
    ) -> bool {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let Some(draft) = drafts.get_mut(&(account_id.to_owned(), room_id.to_owned())) else {
            return false;
        };
        if draft
            .pending
            .as_ref()
            .map(|pending| pending.transaction_id.as_str())
            != Some(transaction_id)
        {
            return false;
        }
        let pending = draft.pending.take().expect("checked pending transaction");
        if draft.revision != pending.queued_revision || !draft.body.is_empty() {
            return false;
        }
        draft.revision = draft.revision.wrapping_add(1);
        if sent {
            draft.dirty = true;
        } else {
            draft.body = pending.submitted;
            // `mark_queued` ran after a successful encrypted store flush.
            draft.dirty = false;
        }
        true
    }

    pub fn forget_account(&self, account_id: &str) {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .retain(|(id, _), _| id != account_id);
    }

    pub async fn load_if_missing(
        &self,
        manager: &AccountManager,
        account_id: &str,
        room_id: &str,
    ) -> Result<(), MatrixCoreError> {
        let key = (account_id.to_owned(), room_id.to_owned());
        if self
            .by_room
            .lock()
            .expect("draft mutex poisoned")
            .contains_key(&key)
        {
            return Ok(());
        }
        let body = manager.load_composer_draft_for(account_id, room_id).await?;
        // An edit made while the disk read was pending wins over stale storage.
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .entry(key)
            .or_insert(CachedDraft {
                body,
                revision: 0,
                dirty: false,
                pending: None,
            });
        Ok(())
    }

    pub async fn persist_one(
        &self,
        manager: &AccountManager,
        account_id: &str,
        room_id: &str,
    ) -> Result<(), MatrixCoreError> {
        // All writes are serialized so an older edit cannot land after a
        // successful send clears the draft.
        let _write = self.write_lock.lock().await;
        let key = (account_id.to_owned(), room_id.to_owned());
        let Some((body, revision)) = self
            .by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key)
            .filter(|draft| draft.dirty)
            .map(|draft| (draft.body.clone(), draft.revision))
        else {
            return Ok(());
        };
        manager
            .save_composer_draft_for(account_id, room_id, &body)
            .await?;
        if let Some(draft) = self
            .by_room
            .lock()
            .expect("draft mutex poisoned")
            .get_mut(&key)
        {
            if draft.revision == revision {
                draft.dirty = false;
            }
        }
        Ok(())
    }

    pub async fn flush_dirty(&self, manager: &AccountManager) -> Result<(), MatrixCoreError> {
        loop {
            let dirty = self.dirty_keys();
            if dirty.is_empty() {
                return Ok(());
            }
            for (account, room) in dirty {
                self.persist_one(manager, &account, &room).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DraftStore;

    #[test]
    fn drafts_are_scoped_and_failed_send_does_not_clear_them() {
        let drafts = DraftStore::default();
        drafts.set("account-a", "room-1", "unsent message");
        drafts.set("account-a", "room-2", "another room");
        drafts.set("account-b", "room-1", "another account");

        // A failed send does not discard the cached text.
        assert_eq!(drafts.get("account-a", "room-1"), "unsent message");
        assert_eq!(drafts.get("account-a", "room-2"), "another room");
        assert_eq!(drafts.get("account-b", "room-1"), "another account");

        drafts.set("account-a", "room-1", "edited while sending");
        assert_eq!(drafts.get("account-a", "room-1"), "edited while sending");

        drafts.forget_account("account-a");
        assert_eq!(drafts.get("account-a", "room-2"), "");
        assert_eq!(drafts.get("account-b", "room-1"), "another account");
    }

    #[test]
    fn queued_send_restores_only_an_unchanged_composer() {
        let drafts = DraftStore::default();
        drafts.set("account", "room", "one");
        let revision = drafts.revision("account", "room").unwrap();
        assert!(drafts.mark_queued("account", "room", "one", revision, "transaction-1"));
        assert_eq!(drafts.get("account", "room"), "");
        assert!(drafts.resolve_queued("account", "room", "transaction-1", false));
        assert_eq!(drafts.get("account", "room"), "one");

        let revision = drafts.revision("account", "room").unwrap();
        assert!(drafts.mark_queued("account", "room", "one", revision, "transaction-2"));
        drafts.set("account", "room", "new thought");
        assert!(!drafts.resolve_queued("account", "room", "transaction-2", false));
        assert_eq!(drafts.get("account", "room"), "new thought");

        let revision = drafts.revision("account", "room").unwrap();
        assert!(drafts.mark_queued("account", "room", "new thought", revision, "transaction-3"));
        assert!(drafts.resolve_queued("account", "room", "transaction-3", true));
        assert_eq!(drafts.get("account", "room"), "");
        assert_eq!(drafts.dirty_keys(), vec![("account".into(), "room".into())]);
    }
}
