//! Account, room and optional thread scoped composer drafts with durable
//! send-recovery records.
//!
//! The UI cache makes room switches immediate. Persistence uses the Matrix
//! client's encrypted SQLite state store through the typed composer-draft
//! row; no draft body or send record is written to the application config, a
//! plaintext file or a raw store value. Live text, generation, every pending
//! record with its complete typed text, and the room's thread index are one
//! snapshot and one encrypted write.
//!
//! Generations are opaque: every user edit, including an edit back to the same
//! text, gets a fresh one, so a send acknowledgement can only clear the exact
//! submission it belongs to. Each record keeps the text it was created from,
//! so the message of a failed, cancelled or unconfirmed send survives later
//! edits and submissions and can be reviewed, copied or restored one record
//! at a time without touching newer live text. Pending records are owned by
//! nonce and exact SDK transaction id; nothing is adopted by content or time,
//! and an unconfirmed record waits for the user's review instead of being
//! resent or discarded. A row this build cannot read is quarantined with its
//! complete original and blocks sends (and, when too large to carry, saves)
//! in that scope until the user explicitly discards it.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Mutex,
};

use morpheus_matrix::{
    AccountManager, ComposeError, ComposeText, DraftEnvelope, Generation, MatrixCoreError,
    PendingLimitError, PendingRecord, QuarantinedRow, ReadOnlyRow, RecordText, RestoredCompose,
    RowRevision,
};
use tokio::sync::Mutex as AsyncMutex;

pub type DraftKey = (String, String, Option<String>);

/// Characters of a record's text shown beside its status.
const PREVIEW_CHARS: usize = 48;

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
    /// Text kept after its request ended without evidence of delivery while
    /// the composer held newer text. No request belongs to it.
    Kept,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingError {
    /// The composer text or revision changed before the record was created.
    Mismatch,
    /// Saved data in this scope could not be read by this build; it must be
    /// reviewed before another send is recorded here.
    Unreadable,
    /// An earlier send here is unconfirmed and must be reviewed first.
    Unconfirmed,
    Limit(PendingLimitError),
}

impl fmt::Display for PendingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mismatch => f.write_str("The message changed before it could be sent. Try again."),
            Self::Unreadable => f.write_str(
                "Saved send status for this conversation could not be read by this version. Review it before sending again.",
            ),
            Self::Unconfirmed => f.write_str(
                "An earlier message here may not have finished sending. Check the conversation and choose before sending again.",
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
    /// Records that still represent a send (not kept text).
    pub count: usize,
    pub kept: usize,
    pub blocked: bool,
    pub unconfirmed: bool,
    /// Saved data this build cannot read is carried; sends are refused.
    pub quarantined: bool,
    /// The saved row is too large to carry; nothing is written here.
    pub read_only: bool,
}

impl PendingSummary {
    pub fn blocks_sends(&self) -> bool {
        self.unconfirmed || self.quarantined || self.read_only
    }
}

/// One record as shown in the status strip.
#[derive(Clone, Debug)]
pub struct PendingItem {
    pub nonce: Generation,
    pub status: PendingStatus,
    /// Leading characters of the record's text, for recognition only.
    pub preview: String,
    /// The SDK may still hold this request (cancel is meaningful).
    pub has_request: bool,
}

/// Exact ownership needed to cancel or retry one queued request.
#[derive(Clone, Debug)]
pub struct PendingTarget {
    pub nonce: Generation,
    pub transaction_id: String,
}

/// What happened to a record's text when its send ended without delivery.
#[derive(Debug)]
pub struct Resolution {
    /// The record's text was put back into the empty composer.
    pub restored: Option<ComposeText>,
    /// The composer held newer text, so the record's text is kept for the
    /// user to restore, copy or discard.
    pub kept: bool,
    /// The scope's edit revision after this mutation. UI text may be applied
    /// only while the revision is still this one.
    pub revision: u64,
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
    /// The complete typed text this record was created from.
    text: ComposeText,
}

impl PendingDraft {
    fn awaits_send(&self) -> bool {
        self.status != PendingStatus::Kept
    }
}

enum Quarantine {
    /// The complete original row rides in the envelope of every save.
    Carried(QuarantinedRow),
    /// The original row cannot be carried completely or rewritten faithfully
    /// (too large, attachments, or a draft kind this build does not write);
    /// the scope is never written.
    ReadOnly(ReadOnlyRow),
}

/// The unreadable original of one scope with the opaque identity the core
/// minted for exactly that complete stored row. A discard names the identity;
/// the core compares the complete row again before releasing anything.
///
/// Invariant: while a selection stays current, a cached scope's original is
/// never replaced. It is set only when the scope enters the cache
/// (`from_restored`, a vacant entry) and cleared only by the explicit
/// `discard_quarantine`; a re-seed of an occupied scope re-statuses records
/// and leaves it alone, and the whole entry disappears only with
/// `forget_account` on logout, which follows the selection's revocation. So
/// the identity a discard reads from the cache when it runs is the identity
/// of the original the notice was rendered for, or none.
struct Blocked {
    kind: Quarantine,
    revision: RowRevision,
}

struct CachedDraft {
    /// Live composer text.
    text: ComposeText,
    /// In-session edit counter used by UI completions.
    revision: u64,
    /// Durable opaque generation; fresh on every user edit.
    generation: Generation,
    /// Bumped on every change that must reach the encrypted row.
    serial: u64,
    dirty: bool,
    pending: Vec<PendingDraft>,
    /// Room scope only: thread roots whose rows hold records.
    threads: Vec<String>,
    quarantine: Option<Blocked>,
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
            threads: Vec::new(),
            quarantine: None,
        }
    }
}

fn preview(text: &ComposeText) -> String {
    let mut out: String = text.body.chars().take(PREVIEW_CHARS).collect();
    if text.body.chars().nth(PREVIEW_CHARS).is_some() {
        out.push('…');
    }
    out
}

impl CachedDraft {
    /// A user edit: new revision and a fresh opaque generation, even when the
    /// text equals an earlier value.
    fn edited(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.generation = Generation::fresh();
        self.changed();
    }

    /// A metadata change; the text generation is unchanged.
    fn changed(&mut self) {
        self.serial = self.serial.wrapping_add(1);
        self.dirty = true;
    }

    fn read_only(&self) -> bool {
        matches!(
            self.quarantine,
            Some(Blocked {
                kind: Quarantine::ReadOnly(_),
                ..
            })
        )
    }

    fn envelope(&self) -> Result<DraftEnvelope, ComposeError> {
        let mut records = Vec::with_capacity(self.pending.len());
        for pending in &self.pending {
            let text = RecordText::from_compose(&pending.text)
                .map_err(|_| ComposeError::InvalidRecovery)?;
            records.push(match (&pending.transaction_id, pending.status) {
                (_, PendingStatus::Kept) => {
                    PendingRecord::kept(pending.nonce.clone(), pending.generation.clone(), text)
                }
                (None, _) => {
                    PendingRecord::enqueuing(pending.nonce.clone(), pending.generation.clone(), text)
                }
                (Some(transaction_id), _) => PendingRecord::queued(
                    pending.nonce.clone(),
                    pending.generation.clone(),
                    transaction_id,
                    text,
                )
                .map_err(|_| ComposeError::InvalidRecovery)?,
            });
        }
        let quarantine = match &self.quarantine {
            Some(Blocked {
                kind: Quarantine::Carried(row),
                ..
            }) => Some(row.clone()),
            _ => None,
        };
        DraftEnvelope::restore(
            self.generation.clone(),
            records,
            self.threads.clone(),
            quarantine,
        )
        .map_err(|_| ComposeError::InvalidRecovery)
    }

    fn summary(&self) -> PendingSummary {
        PendingSummary {
            count: self.pending.iter().filter(|pending| pending.awaits_send()).count(),
            kept: self
                .pending
                .iter()
                .filter(|pending| pending.status == PendingStatus::Kept)
                .count(),
            blocked: self
                .pending
                .iter()
                .any(|pending| matches!(pending.status, PendingStatus::Paused { .. })),
            unconfirmed: self
                .pending
                .iter()
                .any(|pending| pending.status == PendingStatus::Unconfirmed),
            quarantined: matches!(
                self.quarantine,
                Some(Blocked {
                    kind: Quarantine::Carried(_),
                    ..
                })
            ),
            read_only: self.read_only(),
        }
    }

    fn items(&self) -> Vec<PendingItem> {
        self.pending
            .iter()
            .map(|pending| PendingItem {
                nonce: pending.nonce.clone(),
                status: pending.status,
                preview: preview(&pending.text),
                has_request: pending.transaction_id.is_some()
                    && matches!(
                        pending.status,
                        PendingStatus::Queued | PendingStatus::Paused { .. }
                    ),
            })
            .collect()
    }

    /// Put a record's text into the empty composer, provided no edit happened
    /// since the user asked (`expected_revision`). A kept record is consumed;
    /// an awaiting record keeps its copy because its message may still send.
    fn restore_record(&mut self, nonce: &Generation, expected_revision: u64) -> Option<Resolution> {
        if !self.text.body.is_empty() || self.revision != expected_revision {
            return None;
        }
        let index = self.pending.iter().position(|pending| pending.nonce == *nonce)?;
        let text = self.pending[index].text.clone();
        if self.pending[index].status == PendingStatus::Kept {
            self.pending.remove(index);
        }
        self.text = text.clone();
        self.edited();
        Some(Resolution {
            restored: Some(text),
            kept: false,
            revision: self.revision,
        })
    }

    /// The record's send is over without evidence of delivery. Its text goes
    /// back into the composer only while the record still owns the draft
    /// generation (queue acceptance cleared the text without changing it); any
    /// later edit, including one that left the composer empty again, is a
    /// newer draft and the text is kept as its own record instead.
    fn end_without_delivery(&mut self, index: usize) -> Resolution {
        if self.text.body.is_empty() && self.pending[index].generation == self.generation {
            let record = self.pending.remove(index);
            self.text = record.text.clone();
            self.edited();
            Resolution {
                restored: Some(record.text),
                kept: false,
                revision: self.revision,
            }
        } else {
            let record = &mut self.pending[index];
            record.status = PendingStatus::Kept;
            record.transaction_id = None;
            self.changed();
            Resolution {
                restored: None,
                kept: true,
                revision: self.revision,
            }
        }
    }

    fn resolved_sent(&mut self, index: usize) -> Resolution {
        self.pending.remove(index);
        self.changed();
        Resolution {
            restored: None,
            kept: false,
            revision: self.revision,
        }
    }

    fn from_restored(restored: RestoredCompose, attached: Option<&[Generation]>) -> Self {
        let read_only = restored.read_only;
        let revision = restored.revision;
        let envelope = restored.envelope;
        let mut pending = Vec::with_capacity(envelope.pending().len());
        for record in envelope.pending() {
            // A record whose text cannot be rebuilt would have failed the
            // envelope's own validation; this cannot drop a valid record.
            let Ok(text) = record.text().to_compose() else {
                continue;
            };
            pending.push(PendingDraft {
                nonce: record.nonce().clone(),
                generation: record.generation().clone(),
                transaction_id: record.transaction_id().map(str::to_owned),
                // A record from an earlier process is unconfirmed unless the
                // selection-time discovery attached an exact SDK observer.
                status: if record.is_kept() {
                    PendingStatus::Kept
                } else if record.is_queued()
                    && attached.is_some_and(|attached| attached.contains(record.nonce()))
                {
                    PendingStatus::Queued
                } else {
                    PendingStatus::Unconfirmed
                },
                text,
            });
        }
        let kind = match (read_only, envelope.quarantine()) {
            (Some(row), _) => Some(Quarantine::ReadOnly(row)),
            (None, Some(row)) => Some(Quarantine::Carried(row.clone())),
            (None, None) => None,
        };
        // The core mints an identity for every blocked row it restores; a
        // blocked row without one cannot be discarded from here, which is the
        // safe failure.
        let quarantine = match (kind, revision) {
            (Some(kind), Some(revision)) => Some(Blocked { kind, revision }),
            (Some(kind), None) => Some(Blocked {
                kind,
                revision: RowRevision::fresh(),
            }),
            (None, _) => None,
        };
        Self {
            text: restored.text,
            revision: 0,
            generation: envelope.generation().clone(),
            serial: 0,
            dirty: false,
            pending,
            threads: envelope.threads().to_vec(),
            quarantine,
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

    /// Record a send, with its complete typed text, before the SDK sees it.
    /// The composer text and revision must still match the submission; a
    /// thread accepts one awaiting reply, a room a bounded number of records.
    /// Quarantined or unconfirmed earlier status blocks new records until the
    /// user reviews it. The caller must persist before enqueueing.
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
        if draft.quarantine.is_some() {
            return Err(PendingError::Unreadable);
        }
        if draft
            .pending
            .iter()
            .any(|pending| pending.status == PendingStatus::Unconfirmed)
        {
            return Err(PendingError::Unconfirmed);
        }
        let mut envelope = draft.envelope().map_err(|_| PendingError::Mismatch)?;
        let nonce = Generation::fresh();
        envelope.begin_pending(nonce.clone(), draft.generation.clone(), submitted, thread.is_some())?;
        draft.pending.push(PendingDraft {
            nonce: nonce.clone(),
            generation: draft.generation.clone(),
            transaction_id: None,
            status: PendingStatus::Enqueuing,
            text: submitted.clone(),
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
        let Some(index) = draft.pending.iter().position(|pending| {
            pending.nonce == *nonce
                && pending.transaction_id.is_none()
                && pending.status == PendingStatus::Enqueuing
        }) else {
            return false;
        };
        draft.pending.remove(index);
        draft.changed();
        true
    }

    /// Drop an enqueuing record that never reached the row: the cache was
    /// ahead of the row, so removing it reconciles the two. No write is
    /// scheduled and the dirty flag is left as it was, because this is a
    /// reconciliation of the cache, not a change the row has to learn.
    pub fn drop_uncommitted_pending(
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
        let Some(index) = draft.pending.iter().position(|pending| {
            pending.nonce == *nonce
                && pending.transaction_id.is_none()
                && pending.status == PendingStatus::Enqueuing
        }) else {
            return false;
        };
        draft.pending.remove(index);
        true
    }

    /// Record the exact SDK transaction id. Returns whether the submission was
    /// cleared from the composer, which happens only when the composer is
    /// unchanged since `begin_pending`; the record keeps the text either way.
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
        let pending = draft.pending.iter_mut().find(|pending| {
            pending.nonce == handle.nonce
                && pending.transaction_id.is_none()
                && pending.status == PendingStatus::Enqueuing
        })?;
        pending.transaction_id = Some(transaction_id.to_owned());
        pending.status = PendingStatus::Queued;
        let hidden = draft.revision == handle.revision && !draft.text.body.is_empty();
        if hidden {
            let format = draft.text.format;
            draft.text = ComposeText::new(String::new(), format);
            // Clearing the sent submission is not a user edit: the generation
            // stays so an acknowledgement for it still matches exactly.
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
        let Some(pending) = draft
            .pending
            .iter_mut()
            .find(|pending| pending.nonce == *nonce && pending.awaits_send())
        else {
            return false;
        };
        if status == PendingStatus::Kept {
            return false;
        }
        pending.status = status;
        // Statuses are in-session facts; the row is unchanged.
        true
    }

    /// Resolve exactly one owned record. A sent record leaves with its text;
    /// a record that ended without delivery restores its text into an empty
    /// composer or stays as kept text beside newer text.
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
            pending.nonce == *nonce
                && pending.awaits_send()
                && pending.transaction_id.as_deref() == transaction_id
        })?;
        if sent {
            return Some(draft.resolved_sent(index));
        }
        Some(draft.end_without_delivery(index))
    }

    pub fn pending_summary(&self, account: &str, room: &str, thread: Option<&str>) -> PendingSummary {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account, room, thread))
            .map(CachedDraft::summary)
            .unwrap_or_default()
    }

    pub fn pending_items(&self, account: &str, room: &str, thread: Option<&str>) -> Vec<PendingItem> {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account, room, thread))
            .map(CachedDraft::items)
            .unwrap_or_default()
    }

    pub fn thread_send_pending(&self, account: &str, room: &str, root: &str) -> bool {
        self.pending_summary(account, room, Some(root)).count > 0
    }

    /// Exact ownership of one request the SDK may still hold.
    pub fn request_target(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
        nonce: &Generation,
    ) -> Option<PendingTarget> {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account, room, thread))?
            .pending
            .iter()
            .find(|pending| pending.nonce == *nonce)
            .and_then(|pending| match (&pending.transaction_id, pending.status) {
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

    /// The user checked the conversation and decided for one unconfirmed
    /// record. `sent` drops it with its text; otherwise the text returns to an
    /// empty composer or is kept beside newer text. Nothing is sent.
    pub fn review_unconfirmed(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
        nonce: &Generation,
        sent: bool,
    ) -> Option<Resolution> {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let draft = drafts.get_mut(&key(account, room, thread))?;
        let index = draft.pending.iter().position(|pending| {
            pending.nonce == *nonce && pending.status == PendingStatus::Unconfirmed
        })?;
        if sent {
            return Some(draft.resolved_sent(index));
        }
        Some(draft.end_without_delivery(index))
    }

    /// Put one record's text back into an empty composer whose revision is
    /// still `expected_revision`. Kept text is consumed; an awaiting record
    /// keeps its copy since it may still send.
    pub fn restore_record_text(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
        nonce: &Generation,
        expected_revision: u64,
    ) -> Option<Resolution> {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        drafts
            .get_mut(&key(account, room, thread))?
            .restore_record(nonce, expected_revision)
    }

    /// The exact transaction id saved for one record (`Some(None)` for a
    /// record that never received one), as the core release requires.
    pub fn record_transaction_id(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
        nonce: &Generation,
    ) -> Option<Option<String>> {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account, room, thread))?
            .pending
            .iter()
            .find(|pending| pending.nonce == *nonce)
            .map(|pending| pending.transaction_id.clone())
    }

    pub fn record_status(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
        nonce: &Generation,
    ) -> Option<PendingStatus> {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account, room, thread))?
            .pending
            .iter()
            .find(|pending| pending.nonce == *nonce)
            .map(|pending| pending.status)
    }

    /// A copy of one record's text, for the clipboard. The record is unchanged.
    pub fn record_text(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
        nonce: &Generation,
    ) -> Option<ComposeText> {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account, room, thread))?
            .pending
            .iter()
            .find(|pending| pending.nonce == *nonce)
            .map(|pending| pending.text.clone())
    }

    /// Explicitly discard kept text. Only kept records can be discarded; a
    /// record that still represents a send is resolved by its outcome.
    pub fn discard_kept(
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
            .position(|pending| pending.nonce == *nonce && pending.status == PendingStatus::Kept)
        else {
            return false;
        };
        draft.pending.remove(index);
        draft.changed();
        true
    }

    /// Identity of the unreadable original this scope carries or freezes on,
    /// as the core's discard requires.
    pub fn quarantine_revision(&self, account: &str, room: &str, thread: Option<&str>) -> Option<RowRevision> {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account, room, thread))?
            .quarantine
            .as_ref()
            .map(|blocked| blocked.revision.clone())
    }

    /// Explicit user discard of exactly the unreadable original they reviewed,
    /// after the core verified the complete stored row and released it. The
    /// scope becomes writable; the next save replaces the original row. A
    /// different original (changed underneath the review) is left alone.
    pub fn discard_quarantine(&self, account: &str, room: &str, thread: Option<&str>, revision: &RowRevision) -> bool {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let Some(draft) = drafts.get_mut(&key(account, room, thread)) else {
            return false;
        };
        if draft.quarantine.as_ref().map(|blocked| &blocked.revision) != Some(revision) {
            return false;
        }
        draft.quarantine = None;
        draft.changed();
        true
    }

    /// Add a thread root to the room's discovery index. The room scope must be
    /// cached (loaded) first so the room text is never overwritten.
    /// Returns whether the root was newly added (`true`) or already indexed
    /// (`false`, nothing changed); a caller undoes only an entry it added.
    pub fn index_thread(&self, account: &str, room: &str, root: &str) -> Result<bool, PendingError> {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let draft = drafts
            .get_mut(&key(account, room, None))
            .ok_or(PendingError::Mismatch)?;
        if draft.read_only() {
            return Err(PendingError::Unreadable);
        }
        let mut envelope = draft.envelope().map_err(|_| PendingError::Mismatch)?;
        let added = envelope.index_thread(root)?;
        if added {
            draft.threads.push(root.to_owned());
            draft.changed();
        }
        Ok(added)
    }

    /// Drop a thread root from the room index once that thread's cached scope
    /// holds no record of any kind. Returns whether the room row changed.
    pub fn unindex_thread_if_idle(&self, account: &str, room: &str, root: &str) -> bool {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let idle = drafts
            .get(&key(account, room, Some(root)))
            .is_some_and(|thread| thread.pending.is_empty() && thread.quarantine.is_none());
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
    ///
    /// `epoch` is the account epoch the caller read before it started waiting
    /// for discovery: if the account was forgotten in between, nothing is
    /// inserted, so erased plaintext never returns to the cache.
    pub fn seed_recovered(
        &self,
        account_id: &str,
        room_id: &str,
        thread: Option<&str>,
        restored: RestoredCompose,
        attached: &[Generation],
        epoch: u64,
    ) {
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
            read_only: None,
            revision: None,
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
                // A re-selection re-seeds a scope the cache kept across the
                // switch. Every awaiting record from before it is a restored
                // record now: queued only where discovery attached the exact
                // SDK request, otherwise unconfirmed, including a record that
                // never received its id (its grant died with the old epoch).
                // Kept text stays kept.
                for pending in &mut entry.get_mut().pending {
                    if !pending.awaits_send() {
                        continue;
                    }
                    pending.status = if pending.transaction_id.is_some()
                        && attached.contains(&pending.nonce)
                    {
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

    /// Write one scope's live text, generation, records and index as a single
    /// encrypted row; delete the row only when nothing at all remains. A
    /// read-only scope is never written: its original row stays in place and
    /// the cache simply stops reporting it dirty.
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
        let snapshot = {
            let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
            let Some(draft) = drafts.get_mut(&key).filter(|draft| draft.dirty) else {
                return Ok(());
            };
            if draft.read_only() {
                draft.dirty = false;
                return Ok(());
            }
            (draft.text.clone(), draft.envelope(), draft.serial)
        };
        let (text, envelope, serial) = snapshot;
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
            .map(|draft| (draft.text.clone(), draft.envelope().unwrap()))
    }

    #[cfg(test)]
    fn is_dirty(&self, account: &str, room: &str, thread: Option<&str>) -> bool {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&key(account, room, thread))
            .is_some_and(|draft| draft.dirty)
    }

    #[cfg(test)]
    fn clear_dirty_like_a_write_would(&self, account: &str, room: &str, thread: Option<&str>) -> bool {
        // Mirrors `persist_scoped` without a manager: read-only scopes drop
        // the dirty flag without producing anything to write.
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let Some(draft) = drafts.get_mut(&key(account, room, thread)) else {
            return false;
        };
        let writes = draft.dirty && !draft.read_only();
        draft.dirty = false;
        writes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpheus_matrix::TextFormat;

    const TXN_ONE: &str = "11111111111111111111111111111111";
    const TXN_TWO: &str = "22222222222222222222222222222222";

    fn text(body: &str) -> ComposeText {
        ComposeText::plain(body)
    }

    fn typed() -> ComposeText {
        ComposeText::plain("@casey:example.org look")
            .insert_mention(0..18, morpheus_matrix::MentionTarget::User("@casey:example.org".into()))
            .unwrap()
            .0
            .with_format(TextFormat::Markdown)
    }

    fn begin(drafts: &DraftStore, account: &str, room: &str, thread: Option<&str>, body: &str) -> PendingHandle {
        begin_typed(drafts, account, room, thread, text(body))
    }

    fn begin_typed(drafts: &DraftStore, account: &str, room: &str, thread: Option<&str>, value: ComposeText) -> PendingHandle {
        drafts
            .set_composed_scoped(account, room, thread, value.clone())
            .unwrap();
        let revision = drafts.scoped_revision(account, room, thread).unwrap();
        drafts
            .begin_pending(account, room, thread, &value, revision)
            .unwrap()
    }

    fn reload(drafts: &DraftStore, account: &str, room: &str, thread: Option<&str>, attached: &[Generation]) -> DraftStore {
        // Round-trip through the exact row encoding, as a restart would.
        let (saved, envelope) = drafts.snapshot(account, room, thread).unwrap();
        let html = saved.encode_draft(&envelope).unwrap();
        let restored = ComposeText::restore_draft(saved.body.clone(), Some(&html));
        let fresh = DraftStore::default();
        fresh.seed_recovered(account, room, thread, restored, attached, fresh.account_epoch(account));
        fresh
    }

    fn restore(drafts: &DraftStore, account: &str, room: &str, thread: Option<&str>, nonce: &Generation) -> Option<ComposeText> {
        let revision = drafts.scoped_revision(account, room, thread).unwrap();
        drafts
            .restore_record_text(account, room, thread, nonce, revision)
            .and_then(|resolution| resolution.restored)
    }

    fn statuses(drafts: &DraftStore, account: &str, room: &str, thread: Option<&str>) -> Vec<PendingStatus> {
        drafts
            .pending_items(account, room, thread)
            .iter()
            .map(|item| item.status)
            .collect()
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
        assert_eq!(envelope.pending()[0].text().to_compose().unwrap(), text("hello"));
        assert_eq!(drafts.mark_pending_queued("a", "room", None, &handle, TXN_ONE), Some(true));
        assert_eq!(drafts.get("a", "room"), "", "the submission leaves the composer");
        let (saved, envelope) = drafts.snapshot("a", "room", None).unwrap();
        assert_eq!(saved.body, "", "the live text is empty; the record holds the submission");
        assert_eq!(envelope.pending()[0].transaction_id(), Some(TXN_ONE));
        assert_eq!(envelope.pending()[0].text().to_compose().unwrap(), text("hello"));
        // A same-text edit after queueing is a new generation; the record keeps
        // the generation and text it was created from.
        drafts.set("a", "room", "hello");
        let (saved, envelope) = drafts.snapshot("a", "room", None).unwrap();
        assert_eq!(saved.body, "hello");
        assert_ne!(envelope.generation(), envelope.pending()[0].generation());
        assert!(drafts.resolve_pending("a", "room", None, handle.nonce(), Some(TXN_ONE), true).is_some());
        assert_eq!(drafts.get("a", "room"), "hello", "sent evidence never clears a later edit");
        assert!(drafts.snapshot("a", "room", None).unwrap().1.is_empty(), "acknowledged text is not kept");
    }

    #[test]
    fn two_room_sends_keep_their_own_text_and_resolve_out_of_order() {
        let drafts = DraftStore::default();
        let first = begin_typed(&drafts, "a", "room", None, typed());
        assert_eq!(drafts.mark_pending_queued("a", "room", None, &first, TXN_ONE), Some(true));
        let second = begin(&drafts, "a", "room", None, "second");
        assert_eq!(drafts.mark_pending_queued("a", "room", None, &second, TXN_TWO), Some(true));
        assert_eq!(drafts.pending_summary("a", "room", None).count, 2);
        assert_eq!(drafts.record_text("a", "room", None, first.nonce()).unwrap(), typed());
        assert_eq!(drafts.record_text("a", "room", None, second.nonce()).unwrap().body, "second");
        // Wrong transaction for a nonce never resolves anything.
        assert!(drafts.resolve_pending("a", "room", None, first.nonce(), Some(TXN_TWO), true).is_none());
        assert!(drafts.resolve_pending("a", "room", None, first.nonce(), None, true).is_none());
        // The later one is acknowledged first; the earlier record and its
        // typed text survive.
        assert!(drafts.resolve_pending("a", "room", None, second.nonce(), Some(TXN_TWO), true).is_some());
        assert_eq!(drafts.pending_summary("a", "room", None).count, 1);
        assert_eq!(drafts.request_target("a", "room", None, first.nonce()).unwrap().transaction_id, TXN_ONE);
        // Cancelling the first with an empty composer restores its exact text.
        let resolution = drafts.resolve_pending("a", "room", None, first.nonce(), Some(TXN_ONE), false).unwrap();
        assert_eq!(resolution.restored.unwrap(), typed());
        assert!(!resolution.kept);
        assert_eq!(drafts.get_composed_scoped("a", "room", None), typed());
        assert_eq!(drafts.pending_summary("a", "room", None), PendingSummary::default());
    }

    #[test]
    fn a_failed_send_keeps_its_text_beside_newer_text_until_the_user_decides() {
        let drafts = DraftStore::default();
        let first = begin_typed(&drafts, "a", "room", None, typed());
        drafts.mark_pending_queued("a", "room", None, &first, TXN_ONE);
        drafts.set("a", "room", "typed meanwhile");
        // Cancelled while the composer holds newer text: nothing is overwritten
        // and nothing is lost; the first text is kept.
        let resolution = drafts.resolve_pending("a", "room", None, first.nonce(), Some(TXN_ONE), false).unwrap();
        assert!(resolution.restored.is_none() && resolution.kept);
        assert_eq!(drafts.get("a", "room"), "typed meanwhile");
        let summary = drafts.pending_summary("a", "room", None);
        assert_eq!((summary.count, summary.kept), (0, 1));
        assert!(!drafts.thread_send_pending("a", "room", "unused") && !summary.blocks_sends());
        assert_eq!(drafts.record_text("a", "room", None, first.nonce()).unwrap(), typed());
        assert!(drafts.request_target("a", "room", None, first.nonce()).is_none(), "kept text owns no request");
        // Kept text survives restart and further edits, including the same text.
        drafts.set("a", "room", "typed meanwhile");
        let after = reload(&drafts, "a", "room", None, &[]);
        assert_eq!(statuses(&after, "a", "room", None), [PendingStatus::Kept]);
        assert_eq!(after.record_text("a", "room", None, first.nonce()).unwrap(), typed());
        assert_eq!(after.get("a", "room"), "typed meanwhile");
        // Restoring never overwrites the composer; it works once it is empty
        // and consumes the kept record.
        assert!(restore(&after, "a", "room", None, first.nonce()).is_none());
        after.set("a", "room", "");
        // A stale revision (an edit happened since the click) is refused too.
        let stale = after.revision("a", "room").unwrap().wrapping_sub(1);
        assert!(after.restore_record_text("a", "room", None, first.nonce(), stale).is_none());
        assert_eq!(restore(&after, "a", "room", None, first.nonce()).unwrap(), typed());
        assert_eq!(after.get_composed_scoped("a", "room", None), typed());
        assert!(after.snapshot("a", "room", None).unwrap().1.is_empty());
        // Explicit discard is the only other way kept text leaves.
        let other = reload(&drafts, "a", "room", None, &[]);
        assert!(!other.discard_kept("a", "room", None, &Generation::fresh()));
        assert!(other.discard_kept("a", "room", None, first.nonce()));
        assert!(other.snapshot("a", "room", None).unwrap().1.is_empty());
    }

    #[test]
    fn automatic_restore_requires_the_record_to_still_own_the_draft_generation() {
        let drafts = DraftStore::default();
        let handle = begin(&drafts, "a", "room", None, "unsent");
        drafts.mark_pending_queued("a", "room", None, &handle, TXN_ONE);
        // Queue acceptance cleared the text without changing the generation,
        // so a cancellation puts the record's text straight back.
        let same_generation = drafts.generation_of("a", "room", None).unwrap();
        assert_eq!(drafts.get("a", "room"), "");
        let resolution = drafts.resolve_pending("a", "room", None, handle.nonce(), Some(TXN_ONE), false).unwrap();
        assert_eq!(resolution.restored.as_ref().unwrap().body, "unsent");
        assert!(!resolution.kept);
        assert_eq!(drafts.get("a", "room"), "unsent");
        assert_ne!(drafts.generation_of("a", "room", None).unwrap(), same_generation, "the restore is an edit");
        // The user types something newer and clears it again: the composer is
        // empty but belongs to a newer generation. A later cancellation keeps
        // the record's text as its own record instead of restoring it.
        drafts.set("a", "room", "");
        let second = begin(&drafts, "a", "room", None, "second try");
        drafts.mark_pending_queued("a", "room", None, &second, TXN_TWO);
        drafts.set("a", "room", "something newer");
        drafts.set("a", "room", "");
        assert_eq!(drafts.get("a", "room"), "");
        let resolution = drafts.resolve_pending("a", "room", None, second.nonce(), Some(TXN_TWO), false).unwrap();
        assert!(resolution.restored.is_none() && resolution.kept);
        assert_eq!(drafts.get("a", "room"), "", "the newer empty draft is left alone");
        assert_eq!(statuses(&drafts, "a", "room", None), [PendingStatus::Kept]);
        assert_eq!(drafts.record_text("a", "room", None, second.nonce()).unwrap().body, "second try");
        let (saved, envelope) = drafts.snapshot("a", "room", None).unwrap();
        assert_eq!(saved.body, "");
        assert!(envelope.pending()[0].is_kept(), "the kept record is what the row persists");
        // The same holds for an unconfirmed review marked not sent.
        let third = begin(&drafts, "a", "room", None, "third try");
        drafts.set_pending_status("a", "room", None, third.nonce(), PendingStatus::Unconfirmed);
        drafts.set("a", "room", "typed and cleared");
        drafts.set("a", "room", "");
        let resolution = drafts.review_unconfirmed("a", "room", None, third.nonce(), false).unwrap();
        assert!(resolution.kept && drafts.get("a", "room").is_empty());
        // Explicit restore still works at the exact click revision.
        assert_eq!(restore(&drafts, "a", "room", None, third.nonce()).unwrap().body, "third try");
    }

    #[test]
    fn cancellation_restores_text_only_into_an_empty_composer() {
        let drafts = DraftStore::default();
        let handle = begin(&drafts, "a", "room", None, "unsent");
        drafts.mark_pending_queued("a", "room", None, &handle, TXN_ONE);
        let resolution = drafts.resolve_pending("a", "room", None, handle.nonce(), Some(TXN_ONE), false).unwrap();
        assert_eq!(resolution.restored.unwrap().body, "unsent");
        assert_eq!(drafts.get("a", "room"), "unsent");
        // A record whose request may still complete can be restored for
        // editing without losing the record; the copy stays with it.
        let handle = begin(&drafts, "a", "room", None, "unsent");
        drafts.mark_pending_queued("a", "room", None, &handle, TXN_ONE);
        assert_eq!(restore(&drafts, "a", "room", None, handle.nonce()).unwrap().body, "unsent");
        assert_eq!(drafts.pending_summary("a", "room", None).count, 1);
        assert!(drafts.resolve_pending("a", "room", None, handle.nonce(), Some(TXN_ONE), true).is_some());
        assert_eq!(drafts.get("a", "room"), "unsent", "the restored copy is the user's now");
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
        // The index is dropped only once the thread holds no record at all,
        // kept text included.
        assert!(!drafts.unindex_thread_if_idle("a", "room", "$root:example.org"));
        drafts.mark_pending_queued("a", "room", Some("$root:example.org"), &handle, TXN_ONE);
        drafts.set_scoped("a", "room", Some("$root:example.org"), "newer reply");
        let resolution = drafts.resolve_pending("a", "room", Some("$root:example.org"), handle.nonce(), Some(TXN_ONE), false).unwrap();
        assert!(resolution.kept);
        assert!(!drafts.thread_send_pending("a", "room", "$root:example.org"), "kept text does not block the thread");
        assert!(!drafts.unindex_thread_if_idle("a", "room", "$root:example.org"));
        assert!(drafts.discard_kept("a", "room", Some("$root:example.org"), handle.nonce()));
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
        assert_eq!(
            statuses(&after, "a", "room", None),
            [PendingStatus::Unconfirmed, PendingStatus::Queued]
        );
        assert_eq!(after.get("a", "room"), "", "the composer stays as it was");
        assert_eq!(after.record_text("a", "room", None, enqueuing.nonce()).unwrap().body, "lost before id");
        assert_eq!(after.record_text("a", "room", None, queued.nonce()).unwrap().body, "queued");
        assert!(after.request_target("a", "room", None, queued.nonce()).is_some());
        // A new send is refused until the unconfirmed record is reviewed.
        assert!(matches!(
            after.begin_pending("a", "room", None, &text(""), after.revision("a", "room").unwrap()),
            Err(PendingError::Unconfirmed)
        ));
        // Nothing attached: queue absence is unknown, never success.
        let cold = reload(&drafts, "a", "room", None, &[]);
        assert_eq!(
            statuses(&cold, "a", "room", None),
            [PendingStatus::Unconfirmed, PendingStatus::Unconfirmed]
        );
        assert!(cold.request_target("a", "room", None, queued.nonce()).is_none());
        // Review is per record. "It was sent" drops only that record; "not
        // sent" puts its exact text back into the empty composer; the next
        // "not sent" finds the composer occupied and keeps its text instead.
        assert!(cold.review_unconfirmed("a", "room", None, enqueuing.nonce(), true).is_some());
        assert!(cold.review_unconfirmed("a", "room", None, enqueuing.nonce(), true).is_none());
        assert_eq!(cold.pending_summary("a", "room", None).count, 1);
        let resolution = cold.review_unconfirmed("a", "room", None, queued.nonce(), false).unwrap();
        assert_eq!(resolution.restored.unwrap().body, "queued");
        assert_eq!(cold.pending_summary("a", "room", None), PendingSummary::default());
        assert_eq!(cold.get("a", "room"), "queued");
        let busy = reload(&drafts, "a", "room", None, &[]);
        busy.set("a", "room", "writing something else");
        let resolution = busy.review_unconfirmed("a", "room", None, queued.nonce(), false).unwrap();
        assert!(resolution.kept);
        assert_eq!(busy.get("a", "room"), "writing something else");
        assert_eq!(busy.record_text("a", "room", None, queued.nonce()).unwrap().body, "queued");
    }

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
            read_only: None,
        };
        drafts.seed_recovered("a", "room", None, restored, &[second.nonce().clone()], drafts.account_epoch("a"));
        assert_eq!(
            statuses(&drafts, "a", "room", None),
            [PendingStatus::Unconfirmed, PendingStatus::Queued]
        );
        assert!(drafts.request_target("a", "room", None, second.nonce()).is_some());
        assert!(drafts.begin_pending("a", "room", None, &text(""), drafts.revision("a", "room").unwrap()).is_err());
        // Reviewing the first as not sent restores its own text; the second
        // record is untouched.
        let resolution = drafts.review_unconfirmed("a", "room", None, first.nonce(), false).unwrap();
        assert_eq!(resolution.restored.unwrap().body, "first");
        assert_eq!(statuses(&drafts, "a", "room", None), [PendingStatus::Queued]);
        assert_eq!(drafts.get("a", "room"), "first");
        // A loaded-but-not-discovered scope is left alone by a plain load.
        drafts.insert_loaded_if_missing(key("a", "room", None), text("late"), 0);
        assert_eq!(drafts.get("a", "room"), "first");
    }

    #[test]
    fn quarantined_rows_block_sends_persist_through_edits_and_clear_only_on_explicit_discard() {
        let drafts = DraftStore::default();
        drafts.set("a", "room", "typed");
        let (saved, envelope) = drafts.snapshot("a", "room", None).unwrap();
        let broken = saved.encode_draft(&envelope).unwrap().replace("data-morpheus-meta=\"", "data-morpheus-meta=\"ff");
        let restored = ComposeText::restore_draft(saved.body.clone(), Some(&broken));
        assert_eq!(restored.blocked(), Some(morpheus_matrix::QuarantineReason::Malformed));
        let fresh = DraftStore::default();
        fresh.seed_recovered("a", "room", None, restored, &[], fresh.account_epoch("a"));
        assert_eq!(fresh.get("a", "room"), "typed");
        let summary = fresh.pending_summary("a", "room", None);
        assert!(summary.quarantined && !summary.read_only && summary.blocks_sends());
        assert!(matches!(
            fresh.begin_pending("a", "room", None, &text("typed"), fresh.revision("a", "room").unwrap()),
            Err(PendingError::Unreadable)
        ));
        // Edits persist and keep carrying the complete original row, through
        // another restart as well.
        fresh.set("a", "room", "typed more");
        let (_, envelope) = fresh.snapshot("a", "room", None).unwrap();
        assert_eq!(envelope.quarantine().unwrap().original_html().as_deref(), Some(broken.as_str()));
        let again = reload(&fresh, "a", "room", None, &[]);
        assert_eq!(again.get("a", "room"), "typed more");
        assert!(again.pending_summary("a", "room", None).quarantined);
        assert_eq!(
            again.snapshot("a", "room", None).unwrap().1.quarantine().unwrap().original_html().as_deref(),
            Some(broken.as_str())
        );
        // Only the explicit discard of exactly this original clears it;
        // reviewing records or naming another original does not.
        assert!(again.review_unconfirmed("a", "room", None, &Generation::fresh(), true).is_none());
        assert!(again.pending_summary("a", "room", None).quarantined);
        let revision = again.quarantine_revision("a", "room", None).unwrap();
        assert!(!again.discard_quarantine("a", "room", None, &RowRevision::fresh()));
        assert!(again.pending_summary("a", "room", None).quarantined);
        assert!(again.discard_quarantine("a", "room", None, &revision));
        assert!(!again.discard_quarantine("a", "room", None, &revision));
        assert!(!again.pending_summary("a", "room", None).blocks_sends());
        assert!(again.snapshot("a", "room", None).unwrap().1.is_empty());
        assert!(again.is_dirty("a", "room", None));
    }

    #[test]
    fn read_only_rows_are_never_written_until_explicitly_discarded() {
        let restored = RestoredCompose {
            text: text("shown but frozen"),
            discarded_rich_intent: false,
            envelope: DraftEnvelope::fresh(),
            read_only: Some(ReadOnlyRow {
                reason: morpheus_matrix::QuarantineReason::TooLarge,
            }),
            revision: Some(RowRevision::fresh()),
        };
        let revision = restored.revision.clone().unwrap();
        let drafts = DraftStore::default();
        drafts.seed_recovered("a", "room", None, restored, &[], drafts.account_epoch("a"));
        let summary = drafts.pending_summary("a", "room", None);
        assert!(summary.read_only && summary.blocks_sends());
        drafts.set("a", "room", "edited anyway");
        assert!(drafts.is_dirty("a", "room", None));
        assert!(!drafts.clear_dirty_like_a_write_would("a", "room", None), "no write is produced");
        assert!(matches!(
            drafts.begin_pending("a", "room", None, &text("edited anyway"), drafts.revision("a", "room").unwrap()),
            Err(PendingError::Unreadable)
        ));
        assert!(drafts.index_thread("a", "room", "$root:example.org").is_err());
        assert_eq!(drafts.quarantine_revision("a", "room", None), Some(revision.clone()));
        assert!(!drafts.discard_quarantine("a", "room", None, &RowRevision::fresh()), "a different original is not discarded");
        // Readable text stored with fields this build does not write is the
        // same kind of scope: shown, never written, discarded only exactly.
        let unsupported = RestoredCompose {
            text: text("reply draft from another client"),
            discarded_rich_intent: false,
            envelope: DraftEnvelope::fresh(),
            read_only: Some(ReadOnlyRow {
                reason: morpheus_matrix::QuarantineReason::UnsupportedFields,
            }),
            revision: Some(RowRevision::fresh()),
        };
        drafts.seed_recovered("a", "other", None, unsupported, &[], drafts.account_epoch("a"));
        assert_eq!(drafts.get("a", "other"), "reply draft from another client");
        assert!(drafts.pending_summary("a", "other", None).read_only);
        drafts.set("a", "other", "typed over it");
        assert!(!drafts.clear_dirty_like_a_write_would("a", "other", None), "the stored row is never rewritten");
        assert!(drafts.discard_quarantine("a", "room", None, &revision));
        assert!(drafts.clear_dirty_like_a_write_would("a", "room", None), "writes resume after the explicit discard");
        assert!(!drafts.pending_summary("a", "room", None).blocks_sends());
    }

    #[test]
    fn re_seeding_a_kept_scope_marks_every_awaiting_record_restored_and_keeps_kept_text() {
        let drafts = DraftStore::default();
        drafts.set("a", "room", "first");
        let enqueuing = begin(&drafts, "a", "room", None, "first");
        drafts.set("a", "room", "second");
        let queued = begin(&drafts, "a", "room", None, "second");
        drafts.mark_pending_queued("a", "room", None, &queued, &"1".repeat(32));
        drafts.set("a", "room", "third");
        let lost = begin(&drafts, "a", "room", None, "third");
        drafts.mark_pending_queued("a", "room", None, &lost, &"2".repeat(32));
        drafts.set("a", "room", "fourth");
        let kept = begin(&drafts, "a", "room", None, "fourth");
        drafts.mark_pending_queued("a", "room", None, &kept, &"3".repeat(32));
        drafts.set("a", "room", "typed over");
        let kept_txn = "3".repeat(32);
        assert!(drafts.resolve_pending("a", "room", None, kept.nonce(), Some(kept_txn.as_str()), false).unwrap().kept);
        assert_eq!(
            statuses(&drafts, "a", "room", None),
            [PendingStatus::Enqueuing, PendingStatus::Queued, PendingStatus::Queued, PendingStatus::Kept]
        );
        // The cache survived an account switch; the renewed selection seeds
        // the same scope with the exact requests discovery attached.
        let (saved, envelope) = drafts.snapshot("a", "room", None).unwrap();
        let html = saved.encode_draft(&envelope).unwrap();
        let restored = ComposeText::restore_draft(saved.body.clone(), Some(&html));
        drafts.seed_recovered("a", "room", None, restored, &[queued.nonce().clone()], drafts.account_epoch("a"));
        assert_eq!(
            statuses(&drafts, "a", "room", None),
            [PendingStatus::Unconfirmed, PendingStatus::Queued, PendingStatus::Unconfirmed, PendingStatus::Kept],
            "an id-less record is unconfirmed, an unattached queued one too, kept text stays kept"
        );
        assert_eq!(drafts.get("a", "room"), "typed over", "the live text is not replaced");
        assert!(matches!(
            drafts.begin_pending("a", "room", None, &text("typed over"), drafts.revision("a", "room").unwrap()),
            Err(PendingError::Unconfirmed)
        ));
        let _ = enqueuing;
    }

    /// The controller indexes a thread in the room row before it creates the
    /// thread's record, because the background writer may persist any dirty
    /// scope at any moment. At the cache level that order means: after
    /// indexing, only the room scope is dirty and a writer's snapshot of the
    /// thread scope holds no record; the record exists only after the room
    /// write returned, and a room that refuses the index yields no record.
    #[test]
    fn a_writer_snapshot_before_the_room_index_is_durable_never_sees_a_thread_record() {
        let drafts = DraftStore::default();
        let root = "$root:example.org";
        drafts.set("a", "room", "room text");
        drafts.set_scoped("a", "room", Some(root), "reply");
        drafts.clear_dirty_like_a_write_would("a", "room", None);
        drafts.clear_dirty_like_a_write_would("a", "room", Some(root));
        // Step one: index. Only the room is dirty; a writer taking the thread
        // scope now finds nothing pending and nothing to write.
        assert_eq!(drafts.index_thread("a", "room", root), Ok(true));
        assert_eq!(drafts.dirty_keys(), vec![key("a", "room", None)]);
        assert!(drafts.snapshot("a", "room", Some(root)).unwrap().1.pending().is_empty());
        assert!(!drafts.clear_dirty_like_a_write_would("a", "room", Some(root)), "a forced thread write has nothing new");
        // The room write (by the writer or the controller) carries the index.
        assert!(drafts.clear_dirty_like_a_write_would("a", "room", None));
        assert_eq!(drafts.snapshot("a", "room", None).unwrap().1.threads(), [root]);
        // Step two, only now: the record. The thread becomes dirty with its
        // record while the room row already anchors it.
        let handle = begin(&drafts, "a", "room", Some(root), "reply");
        assert_eq!(drafts.dirty_keys(), vec![key("a", "room", Some(root))]);
        assert_eq!(drafts.snapshot("a", "room", Some(root)).unwrap().1.pending().len(), 1);
        let _ = handle;
        // A room that refuses the index (read-only) stops the preparation
        // before any record exists.
        let frozen = DraftStore::default();
        frozen.seed_recovered(
            "a",
            "frozen",
            None,
            RestoredCompose {
                text: text("frozen"),
                discarded_rich_intent: false,
                envelope: DraftEnvelope::fresh(),
                read_only: Some(ReadOnlyRow {
                    reason: morpheus_matrix::QuarantineReason::TooLarge,
                }),
                revision: Some(RowRevision::fresh()),
            },
            &[],
            frozen.account_epoch("a"),
        );
        frozen.set_scoped("a", "frozen", Some(root), "reply");
        assert!(matches!(frozen.index_thread("a", "frozen", root), Err(PendingError::Unreadable)));
        assert!(frozen.snapshot("a", "frozen", Some(root)).unwrap().1.pending().is_empty());
    }

    #[test]
    fn uncommitted_steps_are_dropped_without_scheduling_a_write() {
        let drafts = DraftStore::default();
        drafts.set("a", "room", "text");
        assert!(drafts.clear_dirty_like_a_write_would("a", "room", None));
        let handle = begin(&drafts, "a", "room", None, "text");
        assert!(drafts.is_dirty("a", "room", None), "beginning a record dirties the scope");
        assert!(drafts.clear_dirty_like_a_write_would("a", "room", None), "the write was attempted");
        // The write failed: the record never reached the row. Dropping it
        // reconciles the cache and leaves the dirty flag as it is.
        assert!(drafts.drop_uncommitted_pending("a", "room", None, handle.nonce()));
        assert!(!drafts.is_dirty("a", "room", None));
        assert!(drafts.pending_items("a", "room", None).is_empty());
        assert!(!drafts.drop_uncommitted_pending("a", "room", None, handle.nonce()));
        // A record that has an id is never dropped this way.
        let queued = begin(&drafts, "a", "room", None, "text");
        drafts.mark_pending_queued("a", "room", None, &queued, &"4".repeat(32));
        assert!(!drafts.drop_uncommitted_pending("a", "room", None, queued.nonce()));
        // The index reports whether a root was newly added, so a preparation
        // knows which entry is its own to undo; an existing entry is never
        // removed on its account, and only an added entry dirties the room.
        drafts.clear_dirty_like_a_write_would("a", "room", None);
        assert_eq!(drafts.index_thread("a", "room", "$root:example.org"), Ok(true));
        assert!(drafts.is_dirty("a", "room", None));
        drafts.clear_dirty_like_a_write_would("a", "room", None);
        assert_eq!(drafts.index_thread("a", "room", "$root:example.org"), Ok(false), "already indexed");
        assert!(!drafts.is_dirty("a", "room", None), "re-indexing an existing root changes nothing");
        assert_eq!(
            drafts.snapshot("a", "room", None).unwrap().1.threads(),
            ["$root:example.org"]
        );
        // The idle rule still governs removal: a thread holding a record keeps
        // its entry, an idle one loses it exactly once.
        drafts.set_scoped("a", "room", Some("$root:example.org"), "reply");
        let in_thread = begin(&drafts, "a", "room", Some("$root:example.org"), "reply");
        assert!(!drafts.unindex_thread_if_idle("a", "room", "$root:example.org"));
        assert!(drafts.abandon_pending("a", "room", Some("$root:example.org"), in_thread.nonce()));
        assert!(drafts.unindex_thread_if_idle("a", "room", "$root:example.org"));
        assert!(!drafts.unindex_thread_if_idle("a", "room", "$root:example.org"));
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
        drafts.set("a", "room", "");
        assert!(drafts.resolve_pending("a", "room", None, &Generation::fresh(), Some(TXN_ONE), false).is_none());
    }

    #[test]
    fn stale_queued_marks_keep_edited_text_and_mismatched_records_fail_closed() {
        let drafts = DraftStore::default();
        let handle = begin(&drafts, "a", "room", None, "draft");
        drafts.set("a", "room", "edited during enqueue");
        assert_eq!(drafts.mark_pending_queued("a", "room", None, &handle, TXN_ONE), Some(false));
        assert_eq!(drafts.get("a", "room"), "edited during enqueue");
        assert_eq!(drafts.record_text("a", "room", None, handle.nonce()).unwrap().body, "draft");
        assert!(drafts.mark_pending_queued("a", "room", None, &handle, TXN_TWO).is_none(), "already queued");
        let second = begin(&drafts, "a", "room", None, "edited during enqueue");
        assert!(drafts.mark_pending_queued("a", "room", None, &second, TXN_ONE).is_none(), "duplicate transaction");
        assert!(drafts.mark_pending_queued("a", "room", None, &second, "bad txn!").is_none());
        assert!(matches!(
            drafts.begin_pending("a", "room", None, &text("other"), drafts.revision("a", "room").unwrap()),
            Err(PendingError::Mismatch)
        ));
        assert!(!drafts.set_pending_status("a", "room", None, handle.nonce(), PendingStatus::Kept), "kept is an outcome, not a status");
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
    fn markdown_intent_and_mentions_survive_the_recovery_row_round_trip_per_record() {
        let drafts = DraftStore::default();
        let handle = begin_typed(&drafts, "a", "room", None, typed());
        drafts.mark_pending_queued("a", "room", None, &handle, TXN_ONE);
        // The user types a plain follow-up that replaces the live text.
        drafts.set_composed_scoped("a", "room", None, text("plain follow-up")).unwrap();
        let after = reload(&drafts, "a", "room", None, &[handle.nonce().clone()]);
        assert_eq!(after.get_composed_scoped("a", "room", None), text("plain follow-up"));
        assert_eq!(after.record_text("a", "room", None, handle.nonce()).unwrap(), typed());
        let resolution = after.resolve_pending("a", "room", None, handle.nonce(), Some(TXN_ONE), false).unwrap();
        assert!(resolution.kept);
        after.set("a", "room", "");
        assert_eq!(restore(&after, "a", "room", None, handle.nonce()).unwrap(), typed());
        assert_eq!(after.get_composed_scoped("a", "room", None), typed());
    }

    #[test]
    fn adoption_after_forget_cannot_repopulate_the_erased_cache() {
        // The real adoption path reads the epoch before it waits for the core,
        // then seeds with that epoch. A removal in between must win.
        let drafts = DraftStore::default();
        let handle = begin(&drafts, "a", "room", None, "private");
        drafts.mark_pending_queued("a", "room", None, &handle, TXN_ONE);
        let (saved, envelope) = drafts.snapshot("a", "room", None).unwrap();
        let restored = ComposeText::restore_draft(saved.body.clone(), Some(&saved.encode_draft(&envelope).unwrap()));
        let epoch_before_wait = drafts.account_epoch("a");
        drafts.forget_account("a");
        drafts.seed_recovered("a", "room", None, restored, &[handle.nonce().clone()], epoch_before_wait);
        assert_eq!(drafts.pending_summary("a", "room", None), PendingSummary::default());
        assert_eq!(drafts.get("a", "room"), "");
        assert!(drafts.record_text("a", "room", None, handle.nonce()).is_none());
        // With the current epoch the same seed is accepted: the check is the
        // epoch, not the data.
        let again = DraftStore::default();
        let (saved, envelope) = (saved, envelope);
        let restored = ComposeText::restore_draft(saved.body.clone(), Some(&saved.encode_draft(&envelope).unwrap()));
        again.seed_recovered("a", "room", None, restored, &[handle.nonce().clone()], again.account_epoch("a"));
        assert_eq!(again.pending_summary("a", "room", None).count, 1);
    }

    #[test]
    fn previews_are_short_and_items_carry_exact_ownership() {
        let drafts = DraftStore::default();
        let long = "x".repeat(200);
        let handle = begin(&drafts, "a", "room", None, &long);
        drafts.mark_pending_queued("a", "room", None, &handle, TXN_ONE);
        let items = drafts.pending_items("a", "room", None);
        assert_eq!(items.len(), 1);
        assert_eq!(&items[0].nonce, handle.nonce());
        assert_eq!(items[0].preview.chars().count(), PREVIEW_CHARS + 1);
        assert!(items[0].has_request && items[0].status == PendingStatus::Queued);
        assert!(drafts.set_pending_status("a", "room", None, handle.nonce(), PendingStatus::Paused { recoverable: false }));
        assert_eq!(drafts.paused_targets("a", "room", None).len(), 1);
        assert!(drafts.pending_summary("a", "room", None).blocked);
    }
}
