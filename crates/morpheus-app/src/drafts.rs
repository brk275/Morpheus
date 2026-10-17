//! Account, room and optional thread scoped composer drafts with durable
//! send-recovery records.
//!
//! The UI cache makes room switches immediate. Persistence uses the Matrix
//! client's encrypted SQLite state store through the typed composer-draft
//! row; no draft body or send record is written to the application config, a
//! plaintext file or a raw store value. Text, generation, pending records and
//! the room's thread index are one snapshot and one encrypted write.
//!
//! Generations are opaque: every user edit, including an edit back to the same
//! text, gets a fresh one, so a send acknowledgement can only clear the exact
//! submission it belongs to. Pending records are owned by nonce and exact SDK
//! transaction id; nothing is adopted by content or time, and an unconfirmed
//! record waits for the user's review instead of being resent or discarded.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Mutex,
};

use morpheus_matrix::{
    AccountManager, ComposeError, ComposeText, DraftEnvelope, Generation, MatrixCoreError,
    PendingLimitError, PendingRecord, RestoredCompose,
};
use tokio::sync::Mutex as AsyncMutex;

pub type DraftKey = (String, String, Option<String>);

fn key(account: &str, room: &str, thread: Option<&str>) -> DraftKey {
    (
        account.to_owned(),
        room.to_owned(),
        thread.map(str::to_owned),
    )
}

/// In-session view of one saved send record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingStatus {
    /// Saved before the SDK assigned a transaction id.
    Enqueuing,
    /// The SDK holds the request; its exact transaction id is saved.
    Queued,
    /// The SDK reported a send error and kept the request.
    Paused { recoverable: bool },
    /// No SDK request and no acknowledgement are known for this record. Only
    /// the user's review resolves it.
    Unconfirmed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingError {
    /// The composer text or revision changed before the record was created.
    Mismatch,
    /// Earlier send status in this scope could not be read; it must be
    /// reviewed before another send is recorded here.
    Unreadable,
    Limit(PendingLimitError),
}

impl fmt::Display for PendingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mismatch => f.write_str("The message changed before it could be sent. Try again."),
            Self::Unreadable => f.write_str(
                "Earlier send status for this conversation could not be read. Review it before sending again.",
            ),
            Self::Limit(limit) => write!(f, "{limit}"),
        }
    }
}

impl From<PendingLimitError> for PendingError {
    fn from(limit: PendingLimitError) -> Self {
        Self::Limit(limit)
    }
}

/// Capability returned by `begin_pending`; only its holder can mark the
/// record queued.
#[derive(Clone, Debug)]
pub struct PendingHandle {
    nonce: Generation,
    revision: u64,
}

impl PendingHandle {
    pub fn nonce(&self) -> &Generation {
        &self.nonce
    }
}

/// Fixed facts for the composer status strip; no identifiers or text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PendingSummary {
    pub count: usize,
    pub blocked: bool,
    pub unconfirmed: bool,
    pub retained_available: bool,
    pub metadata_unreadable: bool,
}

/// Exact ownership needed to cancel or retry one queued request.
#[derive(Clone, Debug)]
pub struct PendingTarget {
    pub nonce: Generation,
    pub transaction_id: String,
}

pub struct Resolution {
    /// Retained submission text put back into the empty composer.
    pub restored: Option<ComposeText>,
}

#[derive(Default)]
pub struct DraftStore {
    by_room: Mutex<HashMap<DraftKey, CachedDraft>>,
    account_epochs: Mutex<HashMap<String, u64>>,
    rich_recovery_notices: Mutex<HashSet<DraftKey>>,
    write_lock: AsyncMutex<()>,
}

struct PendingDraft {
    nonce: Generation,
    generation: Generation,
    transaction_id: Option<String>,
    status: PendingStatus,
}

struct RetainedText {
    nonce: Generation,
    text: ComposeText,
}

struct CachedDraft {
    /// Live composer text; empty while a submission is retained.
    text: ComposeText,
    /// In-session edit counter used by UI completions.
    revision: u64,
    /// Durable opaque generation; fresh on every user edit.
    generation: Generation,
    /// Bumped on every change that must reach the encrypted row.
    serial: u64,
    dirty: bool,
    pending: Vec<PendingDraft>,
    retained: Option<RetainedText>,
    /// Room scope only: thread roots whose rows hold pending records.
    threads: Vec<String>,
    /// Saved metadata this build could not read, carried until reviewed.
    unreadable: Option<String>,
}

impl Default for CachedDraft {
    fn default() -> Self {
        Self {
            text: ComposeText::default(),
            revision: 0,
            generation: Generation::fresh(),
            serial: 0,
            dirty: false,
            pending: Vec::new(),
            retained: None,
            threads: Vec::new(),
            unreadable: None,
        }
    }
}

impl CachedDraft {
    /// A user edit: new revision and a fresh opaque generation, even when the
    /// text equals an earlier value.
    fn edited(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.generation = Generation::fresh();
        self.changed();
    }

    /// A metadata or hide/restore change; the text generation is unchanged.
    fn changed(&mut self) {
        self.serial = self.serial.wrapping_add(1);
        self.dirty = true;
    }

    fn envelope(&self) -> Result<DraftEnvelope, ComposeError> {
        let mut records = Vec::with_capacity(self.pending.len());
        for pending in &self.pending {
            records.push(match &pending.transaction_id {
                None => PendingRecord::enqueuing(pending.nonce.clone(), pending.generation.clone()),
                Some(transaction_id) => PendingRecord::queued(
                    pending.nonce.clone(),
                    pending.generation.clone(),
                    transaction_id,
                )
                .map_err(|_| ComposeError::InvalidRecovery)?,
            });
        }
        let retained_for = self
            .retained
            .as_ref()
            .filter(|_| self.text.body.is_empty())
            .filter(|retained| self.pending.iter().any(|pending| pending.nonce == retained.nonce))
            .map(|retained| retained.nonce.clone());
        DraftEnvelope::restore(
            self.generation.clone(),
            retained_for,
            records,
            self.threads.clone(),
            self.unreadable.clone(),
        )
        .map_err(|_| ComposeError::InvalidRecovery)
    }

    /// The text saved in the row: the live text, or the retained submission
    /// while the composer is empty.
    fn persisted_text(&self) -> ComposeText {
        if self.text.body.is_empty()
            && let Some(retained) = &self.retained
            && self.pending.iter().any(|pending| pending.nonce == retained.nonce)
        {
            return retained.text.clone();
        }
        self.text.clone()
    }

    fn summary(&self) -> PendingSummary {
        PendingSummary {
            count: self.pending.len(),
            blocked: self
                .pending
                .iter()
                .any(|pending| matches!(pending.status, PendingStatus::Paused { .. })),
            unconfirmed: self
                .pending
                .iter()
                .any(|pending| pending.status == PendingStatus::Unconfirmed),
            retained_available: self.text.body.is_empty() && self.retained.is_some(),
            metadata_unreadable: self.unreadable.is_some(),
        }
    }

    /// Put the retained submission back into an empty composer.
    fn restore_retained(&mut self) -> Option<ComposeText> {
        if !self.text.body.is_empty() {
            return None;
        }
        let retained = self.retained.take()?;
        self.text = retained.text.clone();
        self.edited();
        Some(retained.text)
    }

    fn from_restored(restored: RestoredCompose, attached: Option<&[Generation]>) -> Self {
        let envelope = restored.envelope;
        let pending: Vec<PendingDraft> = envelope
            .pending()
            .iter()
            .map(|record| PendingDraft {
                nonce: record.nonce().clone(),
                generation: record.generation().clone(),
                transaction_id: record.transaction_id().map(str::to_owned),
                // A record from an earlier process is unconfirmed unless the
                // selection-time discovery attached an exact SDK observer.
                status: if record.is_queued()
                    && attached.is_some_and(|attached| attached.contains(record.nonce()))
                {
                    PendingStatus::Queued
                } else {
                    PendingStatus::Unconfirmed
                },
            })
            .collect();
        let (text, retained) = match envelope.retained_for() {
            Some(nonce) if pending.iter().any(|record| record.nonce == *nonce) => (
                ComposeText::new(String::new(), restored.text.format),
                Some(RetainedText {
                    nonce: nonce.clone(),
                    text: restored.text,
                }),
            ),
            _ => (restored.text, None),
        };
        Self {
            text,
            revision: 0,
            generation: envelope.generation().clone(),
            serial: 0,
            dirty: false,
            pending,
            retained,
            threads: envelope.threads().to_vec(),
            unreadable: envelope.unreadable().map(str::to_owned),
        }
    }
}

impl DraftStore {
    /// Formatting and explicit mention intent share the encrypted draft snapshot.
    pub fn set_markdown(&self, account_id: &str, room_id: &str, enabled: bool) {
        self.set_scoped_markdown(account_id, room_id, None, enabled);
    }

    pub fn set_scoped_markdown(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
        enabled: bool,
    ) {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let draft = drafts
            .entry(key(account, room, thread))
            .or_insert_with(CachedDraft::default);
        let format = if enabled {
            morpheus_matrix::TextFormat::Markdown
        } else {
            morpheus_matrix::TextFormat::Plain
        };
        draft.text = draft.text.with_format(format);
        draft.edited();
    }

    pub fn markdown(&self, account: &str, room: &str) -> bool {
        self.get_composed_scoped(account, room, None).format == morpheus_matrix::TextFormat::Markdown
    }

    pub fn get_composed_scoped(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
    ) -> ComposeText {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account, room, thread))
            .map(|draft| draft.text.clone())
            .unwrap_or_default()
    }

    pub fn set_composed_scoped(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
        text: ComposeText,
    ) -> Result<(), ComposeError> {
        text.validate()?;
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let draft = drafts
            .entry(key(account, room, thread))
            .or_insert_with(CachedDraft::default);
        if !text.body.is_empty() {
            // New text gives up the retained copy of an earlier submission.
            draft.retained = None;
        }
        draft.text = text;
        draft.edited();
        Ok(())
    }

    pub fn take_rich_recovery_notice(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
    ) -> bool {
        self.rich_recovery_notices
            .lock()
            .expect("draft notice mutex poisoned")
            .remove(&key(account, room, thread))
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
            .or_insert_with(CachedDraft::default);
        // `set` is called only for a user edit. Even an edit that ends with
        // the same text must prevent an old failed send from restoring over it.
        entry.text = entry.text.rebase_body(body.to_owned());
        if !body.is_empty() {
            entry.retained = None;
        }
        entry.edited();
    }

    pub fn get(&self, account_id: &str, room_id: &str) -> String {
        self.get_scoped(account_id, room_id, None)
    }

    pub fn get_scoped(&self, account_id: &str, room_id: &str, thread: Option<&str>) -> String {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account_id, room_id, thread))
            .map(|draft| draft.text.body.clone())
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

    /// Record a send before the SDK sees it. The composer text and revision
    /// must still match the submission; a thread accepts one waiting reply,
    /// a room a bounded number. Unreadable earlier status blocks new records
    /// until the user reviews it. The caller must persist before enqueueing.
    pub fn begin_pending(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
        submitted: &ComposeText,
        revision: u64,
    ) -> Result<PendingHandle, PendingError> {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let draft = drafts
            .get_mut(&key(account, room, thread))
            .ok_or(PendingError::Mismatch)?;
        if draft.text != *submitted || draft.revision != revision {
            return Err(PendingError::Mismatch);
        }
        if draft.unreadable.is_some()
            || draft
                .pending
                .iter()
                .any(|pending| pending.status == PendingStatus::Unconfirmed)
        {
            return Err(PendingError::Unreadable);
        }
        let mut envelope = draft.envelope().map_err(|_| PendingError::Mismatch)?;
        let nonce = Generation::fresh();
        envelope.begin_pending(nonce.clone(), draft.generation.clone(), thread.is_some())?;
        draft.pending.push(PendingDraft {
            nonce: nonce.clone(),
            generation: draft.generation.clone(),
            transaction_id: None,
            status: PendingStatus::Enqueuing,
        });
        draft.changed();
        Ok(PendingHandle { nonce, revision })
    }

    /// Drop a record that never reached the SDK (preparation failed before
    /// enqueue). A record with a transaction id is never dropped this way.
    pub fn abandon_pending(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
        nonce: &Generation,
    ) -> bool {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let Some(draft) = drafts.get_mut(&key(account, room, thread)) else {
            return false;
        };
        let Some(index) = draft
            .pending
            .iter()
            .position(|pending| pending.nonce == *nonce && pending.transaction_id.is_none())
        else {
            return false;
        };
        draft.pending.remove(index);
        draft.changed();
        true
    }

    /// Record the exact SDK transaction id. Returns whether the submission was
    /// moved out of the composer into the retained copy, which happens only
    /// when the composer is unchanged since `begin_pending`.
    pub fn mark_pending_queued(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
        handle: &PendingHandle,
        transaction_id: &str,
    ) -> Option<bool> {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let draft = drafts.get_mut(&key(account, room, thread))?;
        if !morpheus_matrix::valid_transaction_id(transaction_id)
            || draft
                .pending
                .iter()
                .any(|pending| pending.transaction_id.as_deref() == Some(transaction_id))
        {
            return None;
        }
        let pending = draft
            .pending
            .iter_mut()
            .find(|pending| pending.nonce == handle.nonce && pending.transaction_id.is_none())?;
        pending.transaction_id = Some(transaction_id.to_owned());
        pending.status = PendingStatus::Queued;
        let hidden = draft.revision == handle.revision && !draft.text.body.is_empty();
        if hidden {
            let format = draft.text.format;
            let submitted = std::mem::replace(&mut draft.text, ComposeText::new(String::new(), format));
            draft.retained = Some(RetainedText {
                nonce: handle.nonce.clone(),
                text: submitted,
            });
            // Hiding is not a user edit: the generation stays with the retained
            // submission so a later acknowledgement can match it exactly.
            draft.revision = draft.revision.wrapping_add(1);
        }
        draft.changed();
        Some(hidden)
    }

    pub fn set_pending_status(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
        nonce: &Generation,
        status: PendingStatus,
    ) -> bool {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let Some(draft) = drafts.get_mut(&key(account, room, thread)) else {
            return false;
        };
        let Some(pending) = draft.pending.iter_mut().find(|pending| pending.nonce == *nonce) else {
            return false;
        };
        if pending.status == status {
            return true;
        }
        pending.status = status;
        // Statuses are in-session facts; the row is unchanged.
        true
    }

    /// Resolve exactly one owned record. A sent record drops its retained
    /// submission and never touches live text; a cancelled record puts the
    /// retained submission back only into an empty composer.
    pub fn resolve_pending(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
        nonce: &Generation,
        transaction_id: Option<&str>,
        sent: bool,
    ) -> Option<Resolution> {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let draft = drafts.get_mut(&key(account, room, thread))?;
        let index = draft.pending.iter().position(|pending| {
            pending.nonce == *nonce && pending.transaction_id.as_deref() == transaction_id
        })?;
        draft.pending.remove(index);
        draft.changed();
        let mut restored = None;
        if draft
            .retained
            .as_ref()
            .is_some_and(|retained| retained.nonce == *nonce)
        {
            if sent {
                draft.retained = None;
            } else {
                restored = draft.restore_retained();
                if restored.is_none() {
                    draft.retained = None;
                }
            }
        }
        Some(Resolution { restored })
    }

    pub fn pending_summary(&self, account: &str, room: &str, thread: Option<&str>) -> PendingSummary {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account, room, thread))
            .map(CachedDraft::summary)
            .unwrap_or_default()
    }

    pub fn thread_send_pending(&self, account: &str, room: &str, root: &str) -> bool {
        self.pending_summary(account, room, Some(root)).count > 0
    }

    /// The most recent request that can still be cancelled or retried.
    pub fn cancellable_target(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
    ) -> Option<PendingTarget> {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account, room, thread))?
            .pending
            .iter()
            .rev()
            .find_map(|pending| match (&pending.transaction_id, pending.status) {
                (Some(transaction_id), PendingStatus::Queued | PendingStatus::Paused { .. }) => {
                    Some(PendingTarget {
                        nonce: pending.nonce.clone(),
                        transaction_id: transaction_id.clone(),
                    })
                }
                _ => None,
            })
    }

    pub fn paused_targets(&self, account: &str, room: &str, thread: Option<&str>) -> Vec<PendingTarget> {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account, room, thread))
            .map(|draft| {
                draft
                    .pending
                    .iter()
                    .filter_map(|pending| match (&pending.transaction_id, pending.status) {
                        (Some(transaction_id), PendingStatus::Paused { .. }) => Some(PendingTarget {
                            nonce: pending.nonce.clone(),
                            transaction_id: transaction_id.clone(),
                        }),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The user checked the conversation and decided for every unconfirmed
    /// record in the scope. `sent` drops the retained submission; otherwise
    /// the retained text returns to an empty composer. Carried unreadable
    /// status is discarded by the same review. Queued records that still have
    /// an observer are untouched.
    pub fn review_unconfirmed(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
        sent: bool,
    ) -> Option<Resolution> {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let draft = drafts.get_mut(&key(account, room, thread))?;
        let before = draft.pending.len();
        let mut reviewed = Vec::new();
        draft.pending.retain(|pending| {
            if pending.status == PendingStatus::Unconfirmed {
                reviewed.push(pending.nonce.clone());
                false
            } else {
                true
            }
        });
        let unreadable = draft.unreadable.take().is_some();
        if draft.pending.len() == before && !unreadable {
            return None;
        }
        let mut restored = None;
        if draft
            .retained
            .as_ref()
            .is_some_and(|retained| reviewed.contains(&retained.nonce))
        {
            if sent {
                draft.retained = None;
            } else {
                restored = draft.restore_retained();
                if restored.is_none() {
                    draft.retained = None;
                }
            }
        }
        draft.changed();
        Some(Resolution { restored })
    }

    /// Put retained submission text back into an empty composer without
    /// resolving its record; the message may still send.
    pub fn restore_retained(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
    ) -> Option<ComposeText> {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        drafts.get_mut(&key(account, room, thread))?.restore_retained()
    }

    /// Add a thread root to the room's discovery index. The room scope must be
    /// cached (loaded) first so the room text is never overwritten.
    pub fn index_thread(&self, account: &str, room: &str, root: &str) -> Result<(), PendingError> {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let draft = drafts
            .get_mut(&key(account, room, None))
            .ok_or(PendingError::Mismatch)?;
        let mut envelope = draft.envelope().map_err(|_| PendingError::Mismatch)?;
        if envelope.index_thread(root)? {
            draft.threads.push(root.to_owned());
            draft.changed();
        }
        Ok(())
    }

    /// Drop a thread root from the room index once that thread's cached scope
    /// holds no record. Returns whether the room row changed.
    pub fn unindex_thread_if_idle(&self, account: &str, room: &str, root: &str) -> bool {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let idle = drafts
            .get(&key(account, room, Some(root)))
            .is_some_and(|thread| thread.pending.is_empty());
        if !idle {
            return false;
        }
        let Some(draft) = drafts.get_mut(&key(account, room, None)) else {
            return false;
        };
        let before = draft.threads.len();
        draft.threads.retain(|known| known != root);
        if draft.threads.len() == before {
            return false;
        }
        draft.changed();
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
        self.rich_recovery_notices
            .lock()
            .expect("draft notice mutex poisoned")
            .retain(|(id, _, _)| id != account_id);
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
        let restored = manager
            .load_composed_draft_for(account_id, room_id, thread)
            .await?;
        // Cache and recovery notice share the logout epoch critical section.
        self.insert_restored_if_missing(key, restored, epoch, None);
        Ok(())
    }

    /// Cache a scope discovered at account selection, marking exactly the
    /// records whose SDK requests were found as queued; all others stay
    /// unconfirmed. A scope already cached keeps its text and records, but its
    /// queued records are re-checked against the discovery: a request the SDK
    /// no longer holds (it may have completed while this account was not
    /// selected and its observers had ended) becomes unconfirmed.
    pub fn seed_recovered(
        &self,
        account_id: &str,
        room_id: &str,
        thread: Option<&str>,
        restored: RestoredCompose,
        attached: &[Generation],
    ) {
        let epoch = self.account_epoch(account_id);
        self.insert_restored_if_missing(key(account_id, room_id, thread), restored, epoch, Some(attached));
    }

    pub fn account_epoch(&self, account_id: &str) -> u64 {
        self.account_epochs
            .lock()
            .expect("draft epoch mutex poisoned")
            .get(account_id)
            .copied()
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn insert_loaded_if_missing(&self, key: DraftKey, text: ComposeText, epoch: u64) {
        let restored = RestoredCompose {
            text,
            discarded_rich_intent: false,
            envelope: DraftEnvelope::fresh(),
            metadata_unreadable: false,
        };
        self.insert_restored_if_missing(key, restored, epoch, None);
    }

    fn insert_restored_if_missing(
        &self,
        key: DraftKey,
        restored: RestoredCompose,
        epoch: u64,
        attached: Option<&[Generation]>,
    ) {
        let epochs = self
            .account_epochs
            .lock()
            .expect("draft epoch mutex poisoned");
        if epochs.get(&key.0).copied().unwrap_or_default() != epoch {
            return;
        }
        let discarded = restored.discarded_rich_intent;
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        match drafts.entry(key.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(CachedDraft::from_restored(restored, attached));
                if discarded {
                    self.rich_recovery_notices
                        .lock()
                        .expect("draft notice mutex poisoned")
                        .insert(key);
                }
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let Some(attached) = attached else {
                    return;
                };
                for pending in &mut entry.get_mut().pending {
                    if pending.transaction_id.is_none() {
                        continue;
                    }
                    pending.status = if attached.contains(&pending.nonce) {
                        PendingStatus::Queued
                    } else {
                        PendingStatus::Unconfirmed
                    };
                }
            }
        }
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

    /// Write one scope's text, generation, records and index as a single
    /// encrypted row; delete the row only when nothing at all remains.
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
        let Some((text, envelope, serial)) = self
            .by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key)
            .filter(|draft| draft.dirty)
            .map(|draft| (draft.persisted_text(), draft.envelope(), draft.serial))
        else {
            return Ok(());
        };
        let envelope = envelope?;
        manager
            .save_composed_draft_for(account_id, room_id, thread, &text, &envelope)
            .await?;
        if let Some(draft) = self
            .by_room
            .lock()
            .expect("draft mutex poisoned")
            .get_mut(&key)
        {
            if draft.serial == serial {
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

    #[cfg(test)]
    fn generation_of(&self, account: &str, room: &str, thread: Option<&str>) -> Option<Generation> {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account, room, thread))
            .map(|draft| draft.generation.clone())
    }

    #[cfg(test)]
    fn snapshot(&self, account: &str, room: &str, thread: Option<&str>) -> Option<(ComposeText, DraftEnvelope)> {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account, room, thread))
            .map(|draft| (draft.persisted_text(), draft.envelope().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpheus_matrix::TextFormat;

    const TXN_ONE: &str = "11111111111111111111111111111111";
    const TXN_TWO: &str = "22222222222222222222222222222222";

    #[test]
    fn reselection_rechecks_cached_queued_records_against_the_sdk_queue() {
        let drafts = DraftStore::default();
        drafts.insert_loaded_if_missing(key("a", "room", None), text(""), 0);
        drafts.set("a", "room", "first");
        let first = drafts
            .begin_pending("a", "room", None, &text("first"), drafts.revision("a", "room").unwrap())
            .unwrap();
        assert_eq!(drafts.mark_pending_queued("a", "room", None, &first, TXN_ONE), Some(true));
        drafts.set("a", "room", "second");
        let second = drafts
            .begin_pending("a", "room", None, &text("second"), drafts.revision("a", "room").unwrap())
            .unwrap();
        assert_eq!(drafts.mark_pending_queued("a", "room", None, &second, TXN_TWO), Some(true));
        // Account switched away and back: the SDK still holds only the second
        // request. The cached text and records stay; the first becomes
        // unconfirmed and is never treated as sent.
        let restored = RestoredCompose {
            text: text("ignored"),
            discarded_rich_intent: false,
            envelope: DraftEnvelope::fresh(),
            metadata_unreadable: false,
        };
        drafts.seed_recovered("a", "room", None, restored, &[second.nonce().clone()]);
        let summary = drafts.pending_summary("a", "room", None);
        assert_eq!((summary.count, summary.unconfirmed), (2, true));
        assert!(drafts.cancellable_target("a", "room", None).is_some_and(|target| target.transaction_id == TXN_TWO));
        assert!(drafts.begin_pending("a", "room", None, &text(""), drafts.revision("a", "room").unwrap()).is_err());
        // Review keeps the queued second record and restores nothing: the
        // retained copy belongs to the second submission.
        let resolution = drafts.review_unconfirmed("a", "room", None, false).unwrap();
        assert!(resolution.restored.is_none());
        assert_eq!(drafts.pending_summary("a", "room", None).count, 1);
        assert_eq!(drafts.get("a", "room"), "");
        // A loaded-but-not-discovered scope is left alone by a plain load.
        drafts.insert_loaded_if_missing(key("a", "room", None), text("late"), 0);
        assert_eq!(drafts.get("a", "room"), "");
    }

    fn text(body: &str) -> ComposeText {
        ComposeText::plain(body)
    }

    fn begin(drafts: &DraftStore, account: &str, room: &str, thread: Option<&str>, body: &str) -> PendingHandle {
        drafts
            .set_composed_scoped(account, room, thread, text(body))
            .unwrap();
        let revision = drafts.scoped_revision(account, room, thread).unwrap();
        drafts
            .begin_pending(account, room, thread, &text(body), revision)
            .unwrap()
    }

    fn reload(drafts: &DraftStore, account: &str, room: &str, thread: Option<&str>, attached: &[Generation]) -> DraftStore {
        // Round-trip through the exact row encoding, as a restart would.
        let (saved, envelope) = drafts.snapshot(account, room, thread).unwrap();
        let html = saved.encode_draft(&envelope).unwrap();
        let restored = ComposeText::restore_draft(saved.body.clone(), Some(&html));
        let fresh = DraftStore::default();
        fresh.seed_recovered(account, room, thread, restored, attached);
        fresh
    }

    #[test]
    fn every_user_edit_gets_a_fresh_generation_even_for_identical_text() {
        let drafts = DraftStore::default();
        drafts.set("a", "room", "same");
        let first = drafts.generation_of("a", "room", None).unwrap();
        drafts.set("a", "room", "same");
        let second = drafts.generation_of("a", "room", None).unwrap();
        assert_ne!(first, second);
        drafts.set_markdown("a", "room", true);
        assert_ne!(drafts.generation_of("a", "room", None).unwrap(), second);
        // Metadata-only changes keep the text generation.
        let handle = begin(&drafts, "a", "room", None, "queued text");
        let before = drafts.generation_of("a", "room", None).unwrap();
        assert_eq!(drafts.mark_pending_queued("a", "room", None, &handle, TXN_ONE), Some(true));
        assert_eq!(drafts.generation_of("a", "room", None).unwrap(), before);
        assert!(drafts.dirty_keys().contains(&key("a", "room", None)));
    }

    #[test]
    fn text_generation_and_records_share_one_row_snapshot() {
        let drafts = DraftStore::default();
        let handle = begin(&drafts, "a", "room", None, "hello");
        let (saved, envelope) = drafts.snapshot("a", "room", None).unwrap();
        assert_eq!(saved.body, "hello");
        assert_eq!(envelope.pending().len(), 1);
        assert!(!envelope.pending()[0].is_queued());
        assert!(envelope.retained_for().is_none());
        assert_eq!(drafts.mark_pending_queued("a", "room", None, &handle, TXN_ONE), Some(true));
        assert_eq!(drafts.get("a", "room"), "");
        let (saved, envelope) = drafts.snapshot("a", "room", None).unwrap();
        assert_eq!(saved.body, "hello", "the submission stays retained in the row");
        assert_eq!(envelope.retained_for(), Some(handle.nonce()));
        assert_eq!(envelope.pending()[0].transaction_id(), Some(TXN_ONE));
        // A same-text edit after queueing is a new generation: the row text
        // becomes the live text and the retained copy is given up.
        drafts.set("a", "room", "hello");
        let (saved, envelope) = drafts.snapshot("a", "room", None).unwrap();
        assert_eq!(saved.body, "hello");
        assert!(envelope.retained_for().is_none());
        assert_ne!(envelope.generation(), envelope.pending()[0].generation());
        assert!(drafts.resolve_pending("a", "room", None, handle.nonce(), Some(TXN_ONE), true).is_some());
        assert_eq!(drafts.get("a", "room"), "hello", "sent evidence never clears a later edit");
        assert!(drafts.snapshot("a", "room", None).unwrap().1.is_empty());
    }

    #[test]
    fn two_room_sends_resolve_independently_and_out_of_order() {
        let drafts = DraftStore::default();
        let first = begin(&drafts, "a", "room", None, "first");
        assert_eq!(drafts.mark_pending_queued("a", "room", None, &first, TXN_ONE), Some(true));
        let second = begin(&drafts, "a", "room", None, "second");
        assert_eq!(drafts.mark_pending_queued("a", "room", None, &second, TXN_TWO), Some(true));
        assert_eq!(drafts.pending_summary("a", "room", None).count, 2);
        // Wrong transaction for a nonce never resolves anything.
        assert!(drafts.resolve_pending("a", "room", None, first.nonce(), Some(TXN_TWO), true).is_none());
        assert!(drafts.resolve_pending("a", "room", None, first.nonce(), None, true).is_none());
        // The later one is acknowledged first; the earlier record survives.
        assert!(drafts.resolve_pending("a", "room", None, second.nonce(), Some(TXN_TWO), true).is_some());
        assert_eq!(drafts.pending_summary("a", "room", None).count, 1);
        assert_eq!(drafts.cancellable_target("a", "room", None).unwrap().transaction_id, TXN_ONE);
        // Cancelling the first restores nothing: its retained copy was replaced by the second submission.
        let resolution = drafts.resolve_pending("a", "room", None, first.nonce(), Some(TXN_ONE), false).unwrap();
        assert!(resolution.restored.is_none());
        assert_eq!(drafts.get("a", "room"), "");
        assert_eq!(drafts.pending_summary("a", "room", None), PendingSummary::default());
    }

    #[test]
    fn cancellation_restores_retained_text_only_into_an_empty_composer() {
        let drafts = DraftStore::default();
        let handle = begin(&drafts, "a", "room", None, "unsent");
        drafts.mark_pending_queued("a", "room", None, &handle, TXN_ONE);
        assert!(drafts.pending_summary("a", "room", None).retained_available);
        let resolution = drafts.resolve_pending("a", "room", None, handle.nonce(), Some(TXN_ONE), false).unwrap();
        assert_eq!(resolution.restored.unwrap().body, "unsent");
        assert_eq!(drafts.get("a", "room"), "unsent");
        let handle = begin(&drafts, "a", "room", None, "unsent");
        drafts.mark_pending_queued("a", "room", None, &handle, TXN_TWO);
        drafts.set("a", "room", "typed meanwhile");
        assert!(!drafts.pending_summary("a", "room", None).retained_available);
        let resolution = drafts.resolve_pending("a", "room", None, handle.nonce(), Some(TXN_TWO), false).unwrap();
        assert!(resolution.restored.is_none());
        assert_eq!(drafts.get("a", "room"), "typed meanwhile");
    }

    #[test]
    fn thread_records_are_single_indexed_in_order_and_preserved_across_room_clears() {
        let drafts = DraftStore::default();
        drafts.set("a", "room", "room text");
        let handle = begin(&drafts, "a", "room", Some("$root:example.org"), "reply");
        drafts.index_thread("a", "room", "$root:example.org").unwrap();
        assert!(matches!(
            drafts.begin_pending("a", "room", Some("$root:example.org"), &text("reply"), drafts.scoped_revision("a", "room", Some("$root:example.org")).unwrap()),
            Err(PendingError::Limit(PendingLimitError::ThreadBusy))
        ));
        assert!(drafts.thread_send_pending("a", "room", "$root:example.org"));
        assert_eq!(drafts.snapshot("a", "room", None).unwrap().1.threads(), ["$root:example.org"]);
        // Clearing the room text keeps a metadata-only row with the index.
        drafts.set("a", "room", "");
        let (saved, envelope) = drafts.snapshot("a", "room", None).unwrap();
        assert_eq!(saved.body, "");
        assert_eq!(envelope.threads().len(), 1);
        assert!(!envelope.is_empty());
        // The index is dropped only once the thread holds no record.
        assert!(!drafts.unindex_thread_if_idle("a", "room", "$root:example.org"));
        drafts.mark_pending_queued("a", "room", Some("$root:example.org"), &handle, TXN_ONE);
        drafts.resolve_pending("a", "room", Some("$root:example.org"), handle.nonce(), Some(TXN_ONE), true);
        assert!(drafts.unindex_thread_if_idle("a", "room", "$root:example.org"));
        assert!(drafts.snapshot("a", "room", None).unwrap().1.is_empty());
        assert!(drafts.index_thread("a", "other-room", "$root:example.org").is_err(), "room scope must be loaded first");
    }

    #[test]
    fn restart_marks_records_unconfirmed_unless_their_request_was_found() {
        let drafts = DraftStore::default();
        let enqueuing = begin(&drafts, "a", "room", None, "lost before id");
        let queued = begin(&drafts, "a", "room", None, "queued");
        drafts.mark_pending_queued("a", "room", None, &queued, TXN_ONE);
        // Discovery found TXN_ONE in the SDK queue.
        let after = reload(&drafts, "a", "room", None, &[queued.nonce().clone()]);
        let summary = after.pending_summary("a", "room", None);
        assert_eq!(summary.count, 2);
        assert!(summary.unconfirmed, "the enqueuing record is unconfirmed");
        assert!(summary.retained_available);
        assert_eq!(after.get("a", "room"), "", "retained submission stays hidden");
        assert!(after.cancellable_target("a", "room", None).is_some());
        // A new send is refused until the unconfirmed record is reviewed.
        assert!(matches!(
            after.begin_pending("a", "room", None, &text(""), after.revision("a", "room").unwrap()),
            Err(PendingError::Unreadable)
        ));
        // Nothing attached: queue absence is unknown, never success.
        let cold = reload(&drafts, "a", "room", None, &[]);
        let summary = cold.pending_summary("a", "room", None);
        assert!(summary.unconfirmed && summary.count == 2);
        assert!(cold.cancellable_target("a", "room", None).is_none());
        assert!(cold.resolve_pending("a", "room", None, enqueuing.nonce(), None, true).is_some());
        // Review restores the retained text only into the empty composer and never sends.
        let resolution = cold.review_unconfirmed("a", "room", None, false).unwrap();
        assert_eq!(resolution.restored.unwrap().body, "queued");
        assert_eq!(cold.pending_summary("a", "room", None), PendingSummary::default());
        assert_eq!(cold.get("a", "room"), "queued");
    }

    #[test]
    fn unreadable_metadata_is_carried_blocks_new_sends_and_clears_only_on_review() {
        let drafts = DraftStore::default();
        drafts.set("a", "room", "typed");
        let (saved, envelope) = drafts.snapshot("a", "room", None).unwrap();
        let html = saved.encode_draft(&envelope).unwrap().replace("data-morpheus-meta=\"", "data-morpheus-meta=\"ff");
        let restored = ComposeText::restore_draft(saved.body.clone(), Some(&html));
        assert!(restored.metadata_unreadable);
        let fresh = DraftStore::default();
        fresh.seed_recovered("a", "room", None, restored, &[]);
        assert_eq!(fresh.get("a", "room"), "typed");
        assert!(fresh.pending_summary("a", "room", None).metadata_unreadable);
        assert!(matches!(
            fresh.begin_pending("a", "room", None, &text("typed"), fresh.revision("a", "room").unwrap()),
            Err(PendingError::Unreadable)
        ));
        // Edits persist and keep carrying the unreadable data.
        fresh.set("a", "room", "typed more");
        assert!(fresh.snapshot("a", "room", None).unwrap().1.unreadable().is_some());
        assert!(fresh.review_unconfirmed("a", "room", None, true).is_some());
        assert!(!fresh.pending_summary("a", "room", None).metadata_unreadable);
        assert!(fresh.snapshot("a", "room", None).unwrap().1.is_empty());
    }

    #[test]
    fn pending_room_sends_are_bounded_and_abandoned_records_vanish() {
        let drafts = DraftStore::default();
        for index in 0..morpheus_matrix::MAX_PENDING_RECORDS {
            let handle = begin(&drafts, "a", "room", None, &format!("message {index}"));
            drafts.mark_pending_queued("a", "room", None, &handle, &format!("{index:0>32}"));
        }
        drafts.set("a", "room", "one more");
        assert!(matches!(
            drafts.begin_pending("a", "room", None, &text("one more"), drafts.revision("a", "room").unwrap()),
            Err(PendingError::Limit(PendingLimitError::TooManyPending))
        ));
        let other = DraftStore::default();
        let handle = begin(&other, "a", "room", None, "never enqueued");
        assert!(other.abandon_pending("a", "room", None, handle.nonce()));
        assert!(!other.abandon_pending("a", "room", None, handle.nonce()));
        assert_eq!(other.get("a", "room"), "never enqueued");
        assert!(other.snapshot("a", "room", None).unwrap().1.is_empty());
        let queued = begin(&other, "a", "room", None, "never enqueued");
        other.mark_pending_queued("a", "room", None, &queued, TXN_ONE);
        assert!(!other.abandon_pending("a", "room", None, queued.nonce()), "a queued record is never abandoned");
    }

    #[test]
    fn stale_queued_marks_keep_edited_text_and_mismatched_records_fail_closed() {
        let drafts = DraftStore::default();
        let handle = begin(&drafts, "a", "room", None, "draft");
        drafts.set("a", "room", "edited during enqueue");
        assert_eq!(drafts.mark_pending_queued("a", "room", None, &handle, TXN_ONE), Some(false));
        assert_eq!(drafts.get("a", "room"), "edited during enqueue");
        assert!(!drafts.pending_summary("a", "room", None).retained_available);
        assert!(drafts.mark_pending_queued("a", "room", None, &handle, TXN_TWO).is_none(), "already queued");
        let second = begin(&drafts, "a", "room", None, "edited during enqueue");
        assert!(drafts.mark_pending_queued("a", "room", None, &second, TXN_ONE).is_none(), "duplicate transaction");
        assert!(drafts.mark_pending_queued("a", "room", None, &second, "bad txn!").is_none());
        assert!(matches!(
            drafts.begin_pending("a", "room", None, &text("other"), drafts.revision("a", "room").unwrap()),
            Err(PendingError::Mismatch)
        ));
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
        assert_eq!(drafts.get_scoped("a", "room", Some("root-1")), "first thread");
        assert_eq!(drafts.get_scoped("a", "room", Some("root-2")), "second thread");
        assert_eq!(drafts.get_scoped("a", "other-room", Some("root-1")), "other room");
        assert_eq!(drafts.get_scoped("b", "room", Some("root-1")), "other account");
        assert_eq!(drafts.dirty_keys().len(), 5);
        drafts.forget_account("a");
        assert_eq!(drafts.get_scoped("a", "room", Some("root-1")), "");
        assert_eq!(drafts.get_scoped("a", "room", Some("root-2")), "");
        assert_eq!(drafts.get("a", "room"), "");
        assert_eq!(drafts.dirty_keys(), vec![key("b", "room", Some("root-1"))]);
    }

    #[test]
    fn delayed_load_cannot_replace_edit_or_repopulate_logged_out_account() {
        let drafts = DraftStore::default();
        let before = drafts.account_epoch("a");
        drafts.set_scoped("a", "room", Some("root"), "typed during load");
        drafts.insert_loaded_if_missing(key("a", "room", Some("root")), "older saved text".into(), before);
        assert_eq!(drafts.get_scoped("a", "room", Some("root")), "typed during load");
        drafts.forget_account("a");
        drafts.insert_loaded_if_missing(key("a", "room", Some("other-root")), "late disk result".into(), before);
        assert_eq!(drafts.scoped_revision("a", "room", Some("other-root")), None);
        drafts.insert_loaded_if_missing(key("b", "room", Some("root")), "restored".into(), drafts.account_epoch("b"));
        assert_eq!(drafts.get_scoped("b", "room", Some("root")), "restored");
        assert!(drafts.dirty_keys().is_empty());
        // Forgetting an account also discards its pending records; a late seed
        // for the old epoch cannot bring them back.
        let handle = begin(&drafts, "b", "room", None, "queued");
        drafts.mark_pending_queued("b", "room", None, &handle, TXN_ONE);
        let (saved, envelope) = drafts.snapshot("b", "room", None).unwrap();
        let restored = ComposeText::restore_draft(saved.body.clone(), Some(&saved.encode_draft(&envelope).unwrap()));
        let epoch = drafts.account_epoch("b");
        drafts.forget_account("b");
        drafts.insert_restored_if_missing(key("b", "room", None), restored, epoch, None);
        assert_eq!(drafts.pending_summary("b", "room", None), PendingSummary::default());
    }

    #[test]
    fn markdown_intent_and_mentions_survive_the_recovery_row_round_trip() {
        let drafts = DraftStore::default();
        let typed = ComposeText::plain("@casey:example.org")
            .insert_mention(0..18, morpheus_matrix::MentionTarget::User("@casey:example.org".into()))
            .unwrap()
            .0
            .with_format(TextFormat::Markdown);
        drafts.set_composed_scoped("a", "room", None, typed.clone()).unwrap();
        let handle = begin(&drafts, "a", "room", None, &typed.body);
        // begin() rebuilt plain text; use the typed submission for the record instead.
        assert!(drafts.abandon_pending("a", "room", None, handle.nonce()));
        drafts.set_composed_scoped("a", "room", None, typed.clone()).unwrap();
        let revision = drafts.revision("a", "room").unwrap();
        let handle = drafts.begin_pending("a", "room", None, &typed, revision).unwrap();
        drafts.mark_pending_queued("a", "room", None, &handle, TXN_ONE);
        let after = reload(&drafts, "a", "room", None, &[handle.nonce().clone()]);
        let resolution = after.resolve_pending("a", "room", None, handle.nonce(), Some(TXN_ONE), false).unwrap();
        assert_eq!(resolution.restored.unwrap(), typed);
        assert_eq!(after.get_composed_scoped("a", "room", None), typed);
    }
}
