//! Account, room and optional thread scoped composer drafts.
//!
//! The UI cache makes room switches immediate. Persistence uses the Matrix
//! client's encrypted SQLite state store; no draft body is written to the
//! application config or to a plaintext fallback file.

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
};

use morpheus_matrix::{AccountManager, MatrixCoreError};
use tokio::sync::Mutex as AsyncMutex;

pub type DraftKey = (String, String, Option<String>);

fn key(account: &str, room: &str, thread: Option<&str>) -> DraftKey {
    (
        account.to_owned(),
        room.to_owned(),
        thread.map(str::to_owned),
    )
}

#[derive(Default)]
pub struct DraftStore {
    by_room: Mutex<HashMap<DraftKey, CachedDraft>>,
    account_epochs: Mutex<HashMap<String, u64>>,
    thread_sends: Mutex<HashSet<DraftKey>>,
    markdown_by_room: Mutex<HashMap<(String, String), bool>>,
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
    /// Formatting is a session-local compose choice. The source text remains
    /// in the encrypted room draft when the app closes.
    pub fn set_markdown(&self, account_id: &str, room_id: &str, enabled: bool) {
        self.markdown_by_room
            .lock()
            .expect("draft formatting mutex poisoned")
            .insert((account_id.to_owned(), room_id.to_owned()), enabled);
    }

    pub fn markdown(&self, account_id: &str, room_id: &str) -> bool {
        self.markdown_by_room
            .lock()
            .expect("draft formatting mutex poisoned")
            .get(&(account_id.to_owned(), room_id.to_owned()))
            .copied()
            .unwrap_or(false)
    }

    pub fn dirty_keys(&self) -> Vec<DraftKey> {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .iter()
            .filter(|(_, draft)| draft.dirty)
            .map(|(key, _)| key.clone())
            .collect()
    }

    pub fn set(&self, account_id: &str, room_id: &str, body: &str) {
        self.set_scoped(account_id, room_id, None, body);
    }

    pub fn set_scoped(&self, account_id: &str, room_id: &str, thread: Option<&str>, body: &str) {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let entry = drafts
            .entry(key(account_id, room_id, thread))
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
        self.get_scoped(account_id, room_id, None)
    }

    pub fn get_scoped(&self, account_id: &str, room_id: &str, thread: Option<&str>) -> String {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account_id, room_id, thread))
            .map(|draft| draft.body.clone())
            .unwrap_or_default()
    }

    pub fn revision(&self, account_id: &str, room_id: &str) -> Option<u64> {
        self.scoped_revision(account_id, room_id, None)
    }

    pub fn scoped_revision(
        &self,
        account_id: &str,
        room_id: &str,
        thread: Option<&str>,
    ) -> Option<u64> {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account_id, room_id, thread))
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
        let Some(draft) = drafts.get_mut(&key(account_id, room_id, None)) else {
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
        let Some(draft) = drafts.get_mut(&key(account_id, room_id, None)) else {
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
        // A disk read begun before logout must not repopulate the erased cache.
        let mut epochs = self
            .account_epochs
            .lock()
            .expect("draft epoch mutex poisoned");
        let epoch = epochs.entry(account_id.to_owned()).or_default();
        *epoch = epoch.wrapping_add(1);
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .retain(|(id, _, _), _| id != account_id);
        self.markdown_by_room
            .lock()
            .expect("draft formatting mutex poisoned")
            .retain(|(id, _), _| id != account_id);
        self.thread_sends
            .lock()
            .expect("thread send mutex poisoned")
            .retain(|(id, _, _)| id != account_id);
    }

    pub fn begin_thread_send(&self, account: &str, room: &str, root: &str) -> bool {
        self.thread_sends
            .lock()
            .expect("thread send mutex poisoned")
            .insert(key(account, room, Some(root)))
    }

    pub fn thread_send_pending(&self, account: &str, room: &str, root: &str) -> bool {
        self.thread_sends
            .lock()
            .expect("thread send mutex poisoned")
            .contains(&key(account, room, Some(root)))
    }

    pub fn finish_thread_send(&self, account: &str, room: &str, root: &str) {
        self.thread_sends
            .lock()
            .expect("thread send mutex poisoned")
            .remove(&key(account, room, Some(root)));
    }

    pub async fn load_if_missing(
        &self,
        manager: &AccountManager,
        account_id: &str,
        room_id: &str,
    ) -> Result<(), MatrixCoreError> {
        self.load_scoped_if_missing(manager, account_id, room_id, None)
            .await
    }

    pub async fn load_scoped_if_missing(
        &self,
        manager: &AccountManager,
        account_id: &str,
        room_id: &str,
        thread: Option<&str>,
    ) -> Result<(), MatrixCoreError> {
        let key = key(account_id, room_id, thread);
        let epoch = self.account_epoch(account_id);
        if self
            .by_room
            .lock()
            .expect("draft mutex poisoned")
            .contains_key(&key)
        {
            return Ok(());
        }
        let body = manager
            .load_scoped_composer_draft_for(account_id, room_id, thread)
            .await?;
        // An edit made while the disk read was pending wins over stale storage.
        self.insert_loaded_if_missing(key, body, epoch);
        Ok(())
    }

    fn account_epoch(&self, account_id: &str) -> u64 {
        self.account_epochs
            .lock()
            .expect("draft epoch mutex poisoned")
            .get(account_id)
            .copied()
            .unwrap_or_default()
    }

    fn insert_loaded_if_missing(&self, key: DraftKey, body: String, epoch: u64) {
        let epochs = self
            .account_epochs
            .lock()
            .expect("draft epoch mutex poisoned");
        if epochs.get(&key.0).copied().unwrap_or_default() != epoch {
            return;
        }
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
    }

    /// A direct thread send clears only the exact submitted revision after a
    /// server acknowledgement. Returning the new revision lets queued UI work
    /// reject edits made even after this cache clear, including identical text.
    pub fn clear_thread_if_unchanged(
        &self,
        account_id: &str,
        room_id: &str,
        thread: &str,
        submitted: &str,
        expected_revision: u64,
    ) -> Option<u64> {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let draft = drafts.get_mut(&key(account_id, room_id, Some(thread)))?;
        if draft.body != submitted || draft.revision != expected_revision {
            return None;
        }
        draft.body.clear();
        draft.revision = draft.revision.wrapping_add(1);
        draft.dirty = true;
        Some(draft.revision)
    }

    pub async fn persist_one(
        &self,
        manager: &AccountManager,
        account_id: &str,
        room_id: &str,
    ) -> Result<(), MatrixCoreError> {
        self.persist_scoped(manager, account_id, room_id, None)
            .await
    }

    pub async fn persist_scoped(
        &self,
        manager: &AccountManager,
        account_id: &str,
        room_id: &str,
        thread: Option<&str>,
    ) -> Result<(), MatrixCoreError> {
        // All writes are serialized so an older edit cannot land after a
        // successful send clears the draft.
        let _write = self.write_lock.lock().await;
        let key = key(account_id, room_id, thread);
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
            .save_scoped_composer_draft_for(account_id, room_id, thread, &body)
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
            for (account, room, thread) in dirty {
                self.persist_scoped(manager, &account, &room, thread.as_deref())
                    .await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DraftStore, key};

    #[test]
    fn pending_thread_send_blocks_only_its_scope_and_keeps_edits() {
        let drafts = DraftStore::default();
        drafts.set_scoped("a", "room", Some("one"), "queued");
        assert!(drafts.begin_thread_send("a", "room", "one"));
        assert!(!drafts.begin_thread_send("a", "room", "one"));
        assert!(drafts.begin_thread_send("a", "room", "two"));
        assert!(drafts.begin_thread_send("a", "another-room", "one"));
        assert!(drafts.begin_thread_send("b", "room", "one"));
        drafts.set_scoped("a", "room", Some("one"), "edited while retained");
        assert_eq!(
            drafts.get_scoped("a", "room", Some("one")),
            "edited while retained"
        );
        drafts.finish_thread_send("a", "room", "one");
        assert!(!drafts.thread_send_pending("a", "room", "one"));
        assert!(drafts.thread_send_pending("a", "room", "two"));
        drafts.forget_account("a");
        assert!(!drafts.thread_send_pending("a", "room", "two"));
        assert!(drafts.thread_send_pending("b", "room", "one"));
    }

    #[test]
    fn thread_drafts_are_isolated_from_rooms_other_threads_and_accounts() {
        let drafts = DraftStore::default();
        drafts.set("a", "room", "room draft");
        drafts.set_scoped("a", "room", Some("root-1"), "first thread");
        drafts.set_scoped("a", "room", Some("root-2"), "second thread");
        drafts.set_scoped("a", "other-room", Some("root-1"), "other room");
        drafts.set_scoped("b", "room", Some("root-1"), "other account");
        assert_eq!(drafts.get("a", "room"), "room draft");
        assert_eq!(
            drafts.get_scoped("a", "room", Some("root-1")),
            "first thread"
        );
        assert_eq!(
            drafts.get_scoped("a", "room", Some("root-2")),
            "second thread"
        );
        assert_eq!(
            drafts.get_scoped("a", "other-room", Some("root-1")),
            "other room"
        );
        assert_eq!(
            drafts.get_scoped("b", "room", Some("root-1")),
            "other account"
        );
        assert_eq!(drafts.dirty_keys().len(), 5);
        drafts.forget_account("a");
        assert_eq!(drafts.get_scoped("a", "room", Some("root-1")), "");
        assert_eq!(drafts.get_scoped("a", "room", Some("root-2")), "");
        assert_eq!(drafts.get("a", "room"), "");
        assert_eq!(drafts.dirty_keys(), vec![key("b", "room", Some("root-1"))]);
    }

    #[test]
    fn thread_acknowledgement_preserves_same_text_edits_and_other_scopes() {
        let drafts = DraftStore::default();
        drafts.set("a", "room", "ordinary");
        drafts.set_scoped("a", "room", Some("root"), "submitted");
        let submitted_revision = drafts.scoped_revision("a", "room", Some("root")).unwrap();
        // A failed send does nothing: the submitted text remains available.
        assert_eq!(drafts.get_scoped("a", "room", Some("root")), "submitted");
        drafts.set_scoped("a", "room", Some("root"), "submitted");
        assert_eq!(
            drafts.clear_thread_if_unchanged("a", "room", "root", "submitted", submitted_revision),
            None
        );
        assert_eq!(drafts.get_scoped("a", "room", Some("root")), "submitted");
        let newer_revision = drafts.scoped_revision("a", "room", Some("root")).unwrap();
        let cleared = drafts
            .clear_thread_if_unchanged("a", "room", "root", "submitted", newer_revision)
            .unwrap();
        assert_eq!(drafts.get_scoped("a", "room", Some("root")), "");
        assert!(
            drafts
                .dirty_keys()
                .contains(&key("a", "room", Some("root")))
        );
        assert_eq!(drafts.get("a", "room"), "ordinary");
        // The UI completion queued after the acknowledgement must also reject
        // a user edit, even one made back to the originally submitted text.
        drafts.set_scoped("a", "room", Some("root"), "submitted");
        assert_ne!(
            drafts.scoped_revision("a", "room", Some("root")),
            Some(cleared)
        );
        assert_eq!(drafts.get_scoped("a", "room", Some("root")), "submitted");
    }

    #[test]
    fn delayed_thread_load_cannot_replace_edit_or_repopulate_logged_out_account() {
        let drafts = DraftStore::default();
        let before = drafts.account_epoch("a");
        drafts.set_scoped("a", "room", Some("root"), "typed during load");
        drafts.insert_loaded_if_missing(
            key("a", "room", Some("root")),
            "older saved text".into(),
            before,
        );
        assert_eq!(
            drafts.get_scoped("a", "room", Some("root")),
            "typed during load"
        );
        drafts.forget_account("a");
        drafts.insert_loaded_if_missing(
            key("a", "room", Some("other-root")),
            "late disk result".into(),
            before,
        );
        assert_eq!(
            drafts.scoped_revision("a", "room", Some("other-root")),
            None
        );
        // A normal fresh disk load starts clean and is immediately cached.
        drafts.insert_loaded_if_missing(
            key("b", "room", Some("root")),
            "restored".into(),
            drafts.account_epoch("b"),
        );
        assert_eq!(drafts.get_scoped("b", "room", Some("root")), "restored");
        assert!(drafts.dirty_keys().is_empty());
    }

    #[test]
    fn drafts_are_scoped_and_failed_send_does_not_clear_them() {
        let drafts = DraftStore::default();
        drafts.set("account-a", "room-1", "unsent message");
        drafts.set("account-a", "room-2", "another room");
        drafts.set("account-b", "room-1", "another account");
        drafts.set_markdown("account-a", "room-1", true);
        assert!(drafts.markdown("account-a", "room-1"));
        assert!(!drafts.markdown("account-a", "room-2"));
        assert!(!drafts.markdown("account-b", "room-1"));

        // A failed send does not discard the cached text.
        assert_eq!(drafts.get("account-a", "room-1"), "unsent message");
        assert_eq!(drafts.get("account-a", "room-2"), "another room");
        assert_eq!(drafts.get("account-b", "room-1"), "another account");

        drafts.set("account-a", "room-1", "edited while sending");
        assert_eq!(drafts.get("account-a", "room-1"), "edited while sending");

        drafts.forget_account("account-a");
        assert_eq!(drafts.get("account-a", "room-2"), "");
        assert_eq!(drafts.get("account-b", "room-1"), "another account");
        assert!(!drafts.markdown("account-a", "room-1"));
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
        assert_eq!(
            drafts.dirty_keys(),
            vec![("account".into(), "room".into(), None)]
        );
    }
}
