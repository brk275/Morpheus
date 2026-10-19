//! Thin presentation adapter between the account-scoped Matrix core and Slint.
//! All Matrix work runs on Tokio; UI models are created on Slint's event loop.

#[cfg(test)]
#[path = "../../morpheus-matrix/tests/support/opened_room.rs"]
mod opened_room;

#[cfg(test)]
#[path = "live_timeline_controller_tests.rs"]
pub(crate) mod timeline_controller_tests;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use morpheus_matrix::{
    AccountInfo, AccountManager, AccountSelection, ComposeSendTarget, ComposeText,
    DownloadedAttachment, EditBaseline, Generation, MatrixCoreError, MatrixLinkTarget, MessageInfo,
    MessageSendState, PublicMediaConsent, QueuedAttachment, QueuedMessage, QueuedSendUpdate,
    RecoveredDraft, RoomInfo, RoomMemberInfo, RoomMembership, StickerCatalog, SyncBackend,
    SyncStatus, TextFormat,
};
use slint::{Color, Model, ModelRc, StyledText, VecModel, Weak};
use tokio::{
    runtime::Handle,
    sync::{Mutex, mpsc},
    task::JoinHandle,
};

use crate::attachments::save_download;
use crate::draft_recovery::{self, PrepareError, SendScope};
use crate::drafts::{
    DraftStore, PendingHandle, PendingItem, PendingStatus, PendingSummary, PendingTarget,
    Reconciled, Resolution, WriteTransaction,
};
use crate::voice_spaces::VoiceSpaces;
use crate::{
    AccountItem, AccountOption, AppWindow, MemberItem, MessageItem, PublicRoomItem, RoomItem,
    SpaceItem,
};

pub enum MessageTarget {
    Room,
    Reply(String),
    Edit(String),
    Thread(String),
}

#[derive(Clone, Copy)]
pub enum RoomAction {
    CreatePrivate,
    CreateSpace,
}

struct KnownSpace {
    name: String,
    recorded_at: Instant,
}

struct MemberRequest {
    account_id: String,
    room_id: String,
    started: Instant,
    epoch: u64,
}

pub struct LiveUi {
    pub manager: Arc<AccountManager>,
    pub(crate) voice_spaces: Arc<VoiceSpaces>,
    pub is_live: Arc<AtomicBool>,
    pub(crate) security_gate: Arc<crate::security::SecurityGate>,
    drafts: Arc<DraftStore>,
    draft_write_tx: mpsc::Sender<()>,
    generation: Arc<AtomicU64>,
    render_epoch: Arc<AtomicU64>,
    navigation_epoch: Arc<AtomicU64>,
    member_epoch: Arc<AtomicU64>,
    member_request: Mutex<Option<MemberRequest>>,
    feed: Mutex<Option<JoinHandle<()>>>,
    feed_key: Mutex<Option<(String, String)>>,
    thread_feed: Mutex<Option<JoinHandle<()>>>,
    thread_generation: Arc<AtomicU64>,
    selected_room: Mutex<Option<String>>,
    selected_space: Mutex<Option<String>>,
    known_spaces: Mutex<HashMap<String, HashMap<String, KnownSpace>>>,
    pub(crate) account_transition: Mutex<()>,
    space_children: Mutex<HashMap<String, HashSet<String>>>,
    space_scan_key: Mutex<Option<(String, Vec<String>)>>,
    space_scan_failed_at: Mutex<Option<Instant>>,
    space_scan_epoch: AtomicU64,
    #[cfg(test)]
    pauses: TestPauses,
}

/// Test-only synchronization points at the real write and authorization
/// boundaries of send preparation and rollback. A test installs a point by
/// name; production code reaching an installed point signals and waits until
/// the test lets it proceed, so an interleaving is forced rather than raced.
/// Points that are not installed cost one mutex lookup. An installed point is
/// one-shot: it is consumed the first time production code reaches it, so a
/// later pass through the same code runs unpaused.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestPauses {
    points: std::sync::Mutex<HashMap<&'static str, Arc<PausePoint>>>,
}

#[cfg(test)]
pub(crate) struct PausePoint {
    entered: tokio::sync::Notify,
    proceed: tokio::sync::Semaphore,
}

#[cfg(test)]
impl TestPauses {
    pub(crate) fn install(&self, name: &'static str) -> Arc<PausePoint> {
        let point = Arc::new(PausePoint {
            entered: tokio::sync::Notify::new(),
            proceed: tokio::sync::Semaphore::new(0),
        });
        self.points
            .lock()
            .expect("pause mutex poisoned")
            .insert(name, point.clone());
        point
    }

    async fn reach(&self, name: &'static str) {
        let point = self
            .points
            .lock()
            .expect("pause mutex poisoned")
            .remove(name);
        if let Some(point) = point {
            point.entered.notify_one();
            point
                .proceed
                .acquire()
                .await
                .expect("pause semaphore closed")
                .forget();
        }
    }
}

#[cfg(test)]
impl PausePoint {
    /// Resolves once production code has reached the point (a stored
    /// notification is kept if it arrived first).
    pub(crate) async fn entered(&self) {
        self.entered.notified().await;
    }

    pub(crate) fn proceed(&self) {
        self.proceed.add_permits(1);
    }
}

/// Which steps of one send preparation happened, and which reached the
/// encrypted row. A step that did not reach the row is the cache's alone and
/// may be reconciled without a write; a step that did is durable and survives
/// the selection that created it. Durable order for a thread send: the room
/// index first, then the thread record, then the grant, so a crash leaves at
/// most an extra empty index entry and never an unindexed record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Prepared {
    /// The enqueuing record was written to its scope's row.
    record: bool,
    /// This preparation added the thread root to the room's cached index; a
    /// root that was already indexed is never this preparation's to remove.
    index_added: bool,
    /// The room index carrying this root was written to the room row.
    index_committed: bool,
}

impl LiveUi {
    pub fn new(
        manager: Arc<AccountManager>,
        runtime: Handle,
        ui: Weak<AppWindow>,
        voice_spaces: Arc<VoiceSpaces>,
    ) -> Arc<Self> {
        let (draft_write_tx, mut draft_write_rx) = mpsc::channel(1);
        let drafts = Arc::new(DraftStore::default());
        let draft_worker_store = drafts.clone();
        let draft_worker_manager = manager.clone();
        runtime.spawn(async move {
            let mut error_reported = false;
            while draft_write_rx.recv().await.is_some() {
                // A single bounded worker coalesces typing bursts. Account
                // switches and app shutdown call flush_drafts directly.
                let mut closed = false;
                loop {
                    match tokio::time::timeout(Duration::from_millis(250), draft_write_rx.recv())
                        .await
                    {
                        Ok(Some(())) => {}
                        Ok(None) => {
                            closed = true;
                            break;
                        }
                        Err(_) => break,
                    }
                }
                let mut failed = false;
                for (account_id, room_id, thread) in draft_worker_store.dirty_keys() {
                    match draft_worker_store
                        .persist_scoped(
                            &draft_worker_manager,
                            &account_id,
                            &room_id,
                            thread.as_deref(),
                        )
                        .await
                    {
                        Ok(()) => {}
                        Err(MatrixCoreError::AccountNotFound(_)) => {}
                        Err(error) => {
                            failed = true;
                            if !error_reported {
                                show_error(
                                    &ui,
                                    format!("Could not save draft on this device: {error}"),
                                );
                            }
                        }
                    }
                }
                error_reported = failed;
                if closed {
                    break;
                }
            }
        });
        Arc::new(Self {
            manager,
            voice_spaces,
            is_live: Arc::new(AtomicBool::new(false)),
            security_gate: Arc::new(crate::security::SecurityGate::default()),
            drafts,
            draft_write_tx,
            generation: Arc::new(AtomicU64::new(0)),
            render_epoch: Arc::new(AtomicU64::new(0)),
            navigation_epoch: Arc::new(AtomicU64::new(0)),
            member_epoch: Arc::new(AtomicU64::new(0)),
            member_request: Mutex::new(None),
            feed: Mutex::new(None),
            feed_key: Mutex::new(None),
            thread_feed: Mutex::new(None),
            thread_generation: Arc::new(AtomicU64::new(0)),
            selected_room: Mutex::new(None),
            selected_space: Mutex::new(None),
            known_spaces: Mutex::new(HashMap::new()),
            account_transition: Mutex::new(()),
            space_children: Mutex::new(HashMap::new()),
            space_scan_key: Mutex::new(None),
            space_scan_failed_at: Mutex::new(None),
            space_scan_epoch: AtomicU64::new(0),
            #[cfg(test)]
            pauses: TestPauses::default(),
        })
    }

    /// Cancel queued presentation work as soon as the user requests a new
    /// account, room, or Space, before the async operation starts running.
    pub fn invalidate_navigation(&self) -> u64 {
        self.render_epoch.fetch_add(1, Ordering::AcqRel);
        self.member_epoch.fetch_add(1, Ordering::AcqRel);
        self.navigation_epoch.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub(crate) fn current_navigation_epoch(&self) -> u64 {
        self.navigation_epoch.load(Ordering::Acquire)
    }

    pub fn composed_draft(&self, account: &str, room: &str, thread: Option<&str>) -> ComposeText {
        self.drafts.get_composed_scoped(account, room, thread)
    }

    pub fn remember_composed_draft(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
        text: ComposeText,
    ) -> Result<(), MatrixCoreError> {
        if !account.is_empty() && !room.is_empty() {
            self.drafts
                .set_composed_scoped(account, room, thread, text)?;
            self.queue_draft_write();
        }
        Ok(())
    }

    pub fn take_draft_recovery_notice(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
    ) -> bool {
        self.drafts.take_rich_recovery_notice(account, room, thread)
    }

    pub fn remember_draft(&self, account_id: &str, room_id: &str, body: &str) {
        if !account_id.is_empty() && !room_id.is_empty() {
            self.drafts.set(account_id, room_id, body);
            self.queue_draft_write();
        }
    }

    pub fn remember_markdown(&self, account_id: &str, room_id: &str, enabled: bool) {
        if !account_id.is_empty() && !room_id.is_empty() {
            self.drafts.set_markdown(account_id, room_id, enabled);
        }
    }

    pub fn remember_thread_draft(&self, account_id: &str, room_id: &str, root: &str, body: &str) {
        if !account_id.is_empty() && !room_id.is_empty() && !root.is_empty() {
            self.drafts
                .set_scoped(account_id, room_id, Some(root), body);
            self.queue_draft_write();
        }
    }

    pub fn thread_draft(&self, account_id: &str, room_id: &str, root: &str) -> String {
        self.drafts.get_scoped(account_id, room_id, Some(root))
    }

    pub fn thread_draft_revision(
        &self,
        account_id: &str,
        room_id: &str,
        root: &str,
    ) -> Option<u64> {
        self.drafts.scoped_revision(account_id, room_id, Some(root))
    }

    pub fn draft_revision(&self, account_id: &str, room_id: &str) -> Option<u64> {
        self.drafts.revision(account_id, room_id)
    }

    pub fn pending_summary(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
    ) -> PendingSummary {
        self.drafts.pending_summary(account, room, thread)
    }

    pub fn pending_items(
        &self,
        account: &str,
        room: &str,
        thread: Option<&str>,
    ) -> Vec<PendingItem> {
        self.drafts.pending_items(account, room, thread)
    }

    pub fn scoped_revision(&self, account: &str, room: &str, thread: Option<&str>) -> Option<u64> {
        self.drafts.scoped_revision(account, room, thread)
    }

    pub fn thread_send_pending(&self, account: &str, room: &str, root: &str) -> bool {
        self.drafts.thread_send_pending(account, room, root)
    }

    /// The current selection for an account, captured synchronously at the
    /// user's action so every later step can be checked against it.
    pub fn account_selection(&self, account: &str) -> Option<AccountSelection> {
        self.manager.account_selection(account)
    }

    /// Exact ownership of one request the SDK may still hold, captured at the
    /// user's action.
    pub fn request_target(&self, scope: &SendScope, nonce: &Generation) -> Option<PendingTarget> {
        self.drafts
            .request_target(&scope.account, &scope.room, scope.thread(), nonce)
    }

    /// A copy of one record's text for the clipboard; the record is unchanged.
    pub fn record_text(&self, scope: &SendScope, nonce: &Generation) -> Option<ComposeText> {
        self.drafts
            .record_text(&scope.account, &scope.room, scope.thread(), nonce)
    }

    /// Record a send and write it to the encrypted draft row before the SDK
    /// sees it, as one writer transaction: for a thread the room's index row
    /// first, then the scope's text, generation and enqueuing record, then the
    /// core's single-use enqueue grant for exactly this record under exactly
    /// this selection. The writer lock is held from before the record exists
    /// in the cache until this returns, so no background flush can commit a
    /// record this preparation has not written itself, and a step that did
    /// not return success did not reach the store. All complete before this
    /// returns. A failure undoes only this preparation's own steps and leaves
    /// the composer text as it was; a selection that ends after a durable step
    /// leaves that step for the next discovery.
    pub async fn prepare_pending_send(
        &self,
        selection: &AccountSelection,
        scope: &SendScope,
        text: &ComposeText,
        revision: u64,
    ) -> Result<PendingHandle, PrepareError> {
        let revoked = || PrepareError::Store(MatrixCoreError::SelectedAccountChanged);
        if !draft_recovery::still_owned(selection, scope) {
            return Err(revoked());
        }
        let (account, room, thread) = (scope.account.as_str(), scope.room.as_str(), scope.thread());
        if thread.is_some() {
            // The room row carries the index; it must be cached before it is
            // rewritten so the room's own text is never lost.
            self.drafts
                .load_if_missing(&self.manager, account, room)
                .await
                .map_err(PrepareError::Store)?;
            // Nothing of ours exists yet; a selection that ended during the
            // read simply stops here.
            if !draft_recovery::still_owned(selection, scope) {
                return Err(revoked());
            }
        }
        // From here until the return no other writer can touch any row. The
        // wait for the lock is an await like any other: a send that queued
        // behind the background writer and whose selection ended meanwhile
        // creates nothing.
        let tx = self.drafts.begin_write_transaction().await;
        if !draft_recovery::still_owned(selection, scope) {
            return Err(revoked());
        }
        let mut prepared = Prepared::default();
        if let Some(root) = thread {
            // The index is made durable before any record exists in the
            // cache; an index entry alone is harmless.
            prepared.index_added = self
                .drafts
                .index_thread(account, room, root)
                .map_err(PrepareError::Record)?;
            if let Err(error) = tx.persist_scoped(&self.manager, account, room, None).await {
                self.undo_added_index(&tx, selection, scope, &prepared)
                    .await;
                return Err(PrepareError::Store(error));
            }
            prepared.index_committed = true;
            #[cfg(test)]
            self.pauses.reach("prepare.after_index_commit").await;
            if !draft_recovery::still_owned(selection, scope) {
                return Err(revoked());
            }
        }
        let handle = match self
            .drafts
            .begin_pending(account, room, thread, text, revision)
        {
            Ok(handle) => handle,
            Err(error) => {
                self.undo_added_index(&tx, selection, scope, &prepared)
                    .await;
                return Err(PrepareError::Record(error));
            }
        };
        #[cfg(test)]
        self.pauses.reach("prepare.after_begin").await;
        if let Err(error) = tx
            .persist_scoped(&self.manager, account, room, thread)
            .await
        {
            // Our write is the only one that could have carried the record
            // and it did not succeed: the record never reached the store.
            self.rollback_pending(&tx, selection, scope, handle.nonce(), &prepared)
                .await;
            return Err(PrepareError::Store(error));
        }
        prepared.record = true;
        // From here the enqueuing record is durable and indexed. If the
        // selection ended while it was written, the row keeps it for the next
        // discovery to review; nothing of the departed account's cache is
        // touched.
        if !draft_recovery::still_owned(selection, scope) {
            return Err(revoked());
        }
        match self
            .manager
            .grant_enqueue_for(selection, room, thread, handle.nonce())
            .await
        {
            Ok(()) if draft_recovery::still_owned(selection, scope) => Ok(handle),
            // The core refused because the selection ended (or it ended right
            // after): the durable record stays for review.
            Ok(()) | Err(MatrixCoreError::SelectedAccountChanged) => Err(revoked()),
            Err(error) => {
                self.rollback_pending(&tx, selection, scope, handle.nonce(), &prepared)
                    .await;
                Err(PrepareError::Store(error))
            }
        }
    }

    /// Undo a failed preparation inside its writer transaction. Only this
    /// call's own steps are undone, and only as far as the selection still
    /// owns them, and the typed submission is never lost. The drop-or-keep
    /// decision is made by the cache at the moment it mutates, under its own
    /// lock, by comparing the record's complete typed text (body, format and
    /// mentions) with what the composer holds right then: while the composer
    /// still holds exactly that submission the record is dropped, since the
    /// text is right there; otherwise the record becomes kept text beside
    /// the newer composer text. No earlier comparison is reused across an
    /// await, so typing while the release waits cannot drop the submission.
    /// `prepared.record == false` means the record never reached the store:
    /// the transaction held the writer lock since before the record existed,
    /// so no other writer could have committed it. Such a record is dropped
    /// from the cache (also after the selection ended) only while the
    /// composer holds it; a typed-over one is retained, dirty, as the
    /// enqueuing record it is, and while the selection still owns the scope
    /// it is made durable before it can be kept. A committed record is
    /// removed or kept only after the core granted its exact release (nonce,
    /// no transaction id) and the selection was re-checked, which is also the
    /// core's one path from an enqueuing record to kept text: the release,
    /// not fresh ownership, authorizes the change, so a revocation during the
    /// write cannot leave the cache and the row disagreeing, and a failed
    /// write is completed by the account-bound flush under that release; a
    /// refused release leaves cache and row alike for discovery. After the
    /// selection ended nothing committed is touched, no text absent from the
    /// composer is deleted, the index entry is never removed, no write is
    /// made and no flush is scheduled: a retained cache-only record reaches
    /// the row through the account-bound flush and the next discovery
    /// reports it as unconfirmed. Each write is followed by an ownership
    /// re-check before the next write or the scheduled flush; the thread row
    /// is written before the room row so the core's dropped-index proof sees
    /// an idle thread, and a thread that keeps text keeps its index entry.
    async fn rollback_pending(
        &self,
        tx: &WriteTransaction<'_>,
        selection: &AccountSelection,
        scope: &SendScope,
        nonce: &Generation,
        prepared: &Prepared,
    ) {
        let (account, room, thread) = (scope.account.as_str(), scope.room.as_str(), scope.thread());
        if !draft_recovery::still_owned(selection, scope) {
            if !prepared.record {
                self.drafts
                    .drop_uncommitted_pending_if_held(account, room, thread, nonce);
            }
            return;
        }
        if !prepared.record {
            if self
                .drafts
                .drop_uncommitted_pending_if_held(account, room, thread, nonce)
            {
                if !self.undo_added_index(tx, selection, scope, prepared).await {
                    return;
                }
                self.queue_draft_write();
                return;
            }
            // The submission exists only in the cache and the composer moved
            // on: make it durable before it can be kept.
            if tx
                .persist_scoped(&self.manager, account, room, thread)
                .await
                .is_err()
            {
                // The store is not reachable now. The text stays in the cache
                // for review; the account-bound flush writes it as the
                // enqueuing record it is and the next discovery reports it as
                // unconfirmed. Nothing is dropped.
                self.drafts.set_pending_status(
                    account,
                    room,
                    thread,
                    nonce,
                    PendingStatus::Unconfirmed,
                );
                return;
            }
            if !draft_recovery::still_owned(selection, scope) {
                return;
            }
        }
        if self
            .manager
            .release_pending_record_for(selection, room, thread, nonce, None)
            .await
            .is_err()
        {
            // No exact authority to change it: the record stays in cache and
            // row alike, and the next discovery reports it as unconfirmed.
            return;
        }
        #[cfg(test)]
        self.pauses.reach("rollback.after_release").await;
        if !draft_recovery::still_owned(selection, scope) {
            return;
        }
        // Decided against the composer text of this very moment.
        let _: Option<Reconciled> = self
            .drafts
            .reconcile_released_pending(account, room, thread, nonce);
        #[cfg(test)]
        self.pauses.reach("rollback.before_write").await;
        // Best effort: the release outlives this write, so a failed write is
        // completed by the account-bound flush; the cache never shows less
        // than the row will.
        let _ = tx
            .persist_scoped(&self.manager, account, room, thread)
            .await;
        if !draft_recovery::still_owned(selection, scope) {
            return;
        }
        if !self.undo_added_index(tx, selection, scope, prepared).await {
            return;
        }
        self.queue_draft_write();
    }

    /// Remove the index entry this preparation added, once its thread is
    /// idle, writing the room row only while the selection still owns the
    /// scope. Returns whether the selection still owned the scope afterwards.
    async fn undo_added_index(
        &self,
        tx: &WriteTransaction<'_>,
        selection: &AccountSelection,
        scope: &SendScope,
        prepared: &Prepared,
    ) -> bool {
        let (account, room, thread) = (scope.account.as_str(), scope.room.as_str(), scope.thread());
        if !draft_recovery::still_owned(selection, scope) {
            return false;
        }
        if let Some(root) = thread
            && prepared.index_added
            && self.drafts.unindex_thread_if_idle(account, room, root)
            && prepared.index_committed
        {
            let _ = tx.persist_scoped(&self.manager, account, room, None).await;
            return draft_recovery::still_owned(selection, scope);
        }
        true
    }

    /// Save the exact SDK transaction id. Returns whether the submission was
    /// cleared from the composer (unchanged since the record was begun).
    /// Nothing is recorded once the selection the send ran under is gone: the
    /// saved enqueuing record is reviewed by the next discovery instead.
    pub async fn record_pending_queued(
        &self,
        selection: &AccountSelection,
        scope: &SendScope,
        handle: &PendingHandle,
        transaction_id: &str,
    ) -> Option<bool> {
        if !draft_recovery::still_owned(selection, scope) {
            return None;
        }
        let (account, room, thread) = (scope.account.as_str(), scope.room.as_str(), scope.thread());
        let hidden =
            self.drafts
                .mark_pending_queued(account, room, thread, handle, transaction_id)?;
        if self
            .drafts
            .persist_scoped(&self.manager, account, room, thread)
            .await
            .is_err()
        {
            // The in-memory record is right; the worker retries the row.
            self.queue_draft_write();
        }
        Some(hidden)
    }

    /// Drop a record whose enqueue failed before the SDK stored anything,
    /// under the selection it was created for.
    pub async fn abandon_pending_send(
        &self,
        selection: &AccountSelection,
        scope: &SendScope,
        nonce: &Generation,
    ) {
        // Both steps reached the row before the SDK boundary refused. The
        // cache is not touched here: the rollback removes the committed record
        // (or keeps its text, if the composer moved on meanwhile) only under
        // the core's exact release, while the selection still owns the scope;
        // otherwise the record stays for the next discovery.
        if !draft_recovery::still_owned(selection, scope)
            || self
                .drafts
                .record_status(&scope.account, &scope.room, scope.thread(), nonce)
                != Some(PendingStatus::Enqueuing)
        {
            return;
        }
        // The index entry was durable before this send or added by its
        // preparation; either way the record it anchored is being dropped,
        // so the idle-thread rule decides whether the entry goes too.
        let prepared = Prepared {
            record: true,
            index_added: scope.thread().is_some(),
            index_committed: scope.thread().is_some(),
        };
        let tx = self.drafts.begin_write_transaction().await;
        self.rollback_pending(&tx, selection, scope, nonce, &prepared)
            .await;
    }

    /// Mark one record for review, under the selection the observation ran
    /// under. A departed selection marks nothing and schedules no write.
    pub async fn mark_pending_unconfirmed(
        &self,
        selection: &AccountSelection,
        scope: &SendScope,
        nonce: &Generation,
    ) {
        if !draft_recovery::still_owned(selection, scope) {
            return;
        }
        self.drafts.set_pending_status(
            &scope.account,
            &scope.room,
            scope.thread(),
            nonce,
            PendingStatus::Unconfirmed,
        );
        self.queue_draft_write();
    }

    /// Write one scope's row (and the room index it may have left) for a
    /// current selection. A departed selection writes nothing: its account's
    /// cache is cleared or re-seeded by the next activation.
    async fn persist_scope_and_index(&self, selection: &AccountSelection, scope: &SendScope) {
        if !draft_recovery::still_owned(selection, scope) {
            return;
        }
        let (account, room, thread) = (scope.account.as_str(), scope.room.as_str(), scope.thread());
        if self
            .drafts
            .persist_scoped(&self.manager, account, room, thread)
            .await
            .is_err()
        {
            if draft_recovery::still_owned(selection, scope) {
                self.queue_draft_write();
            }
            return;
        }
        // The index is touched only while the selection still owns the scope;
        // a switch during the write leaves the room row to the background
        // flush and the next activation.
        if !draft_recovery::still_owned(selection, scope) {
            return;
        }
        if let Some(root) = thread
            && self.drafts.unindex_thread_if_idle(account, room, root)
            && draft_recovery::still_owned(selection, scope)
            && self
                .drafts
                .persist_scoped(&self.manager, account, room, None)
                .await
                .is_err()
        {
            self.queue_draft_write();
        }
    }

    /// Ask the core to release exactly one record (nonce plus the transaction
    /// id the cache holds for it) under the given selection. False means the
    /// core does not own such a record now; the cache is left untouched.
    async fn release_record(
        &self,
        selection: &AccountSelection,
        scope: &SendScope,
        nonce: &Generation,
    ) -> bool {
        let Some(transaction_id) =
            self.drafts
                .record_transaction_id(&scope.account, &scope.room, scope.thread(), nonce)
        else {
            return false;
        };
        selection.is_current()
            && self
                .manager
                .release_pending_record_for(
                    selection,
                    &scope.room,
                    scope.thread(),
                    nonce,
                    transaction_id.as_deref(),
                )
                .await
                .is_ok()
    }

    fn publish_resolution(
        self: &Arc<Self>,
        ui: &Weak<AppWindow>,
        selection: AccountSelection,
        scope: SendScope,
        resolution: Option<Resolution>,
        notice: &'static str,
    ) {
        let live = self.clone();
        let _ = ui.upgrade_in_event_loop(move |window| {
            if !selection.is_current() || !draft_recovery::scope_visible(&window, &scope) {
                return;
            }
            if let Some(resolution) = &resolution {
                draft_recovery::apply_resolution(&window, &live, &scope, resolution);
            }
            draft_recovery::publish_scope(&window, &live, &scope);
            if !notice.is_empty() {
                window.set_transfer_notice(notice.into());
            }
        });
    }

    /// Apply one SDK update to the exact owned record and the visible UI.
    /// Returns whether the update was terminal for that record. Nothing is
    /// changed once the selection the observer runs under is gone: the
    /// selection is checked before the core call and again after it, before
    /// either cache mutation, so a release that outlives the selection (a
    /// switch, a removal or A->B->A) simply ends this observer. The exact
    /// record and transaction id remain the authority for a current one.
    pub(crate) async fn apply_pending_update(
        self: &Arc<Self>,
        selection: &AccountSelection,
        scope: &SendScope,
        nonce: &Generation,
        transaction_id: &str,
        update: QueuedSendUpdate,
        ui: &Weak<AppWindow>,
    ) -> bool {
        if !draft_recovery::still_owned(selection, scope) {
            return true;
        }
        let (account, room, thread) = (scope.account.as_str(), scope.room.as_str(), scope.thread());
        let (terminal, resolution) = match update {
            QueuedSendUpdate::Sent | QueuedSendUpdate::Cancelled => {
                let sent = update == QueuedSendUpdate::Sent;
                let released = self
                    .manager
                    .release_pending_record_for(
                        selection,
                        room,
                        thread,
                        nonce,
                        Some(transaction_id),
                    )
                    .await;
                if !draft_recovery::still_owned(selection, scope) {
                    // The selection ended while the core call ran: whatever
                    // it returned, this observer is stale and changes nothing.
                    return true;
                }
                match released {
                    Ok(()) => {}
                    Err(MatrixCoreError::SelectedAccountChanged) => return true,
                    Err(_) => {
                        // The core does not own this record any more (the row
                        // was resolved another way): leave the cache for
                        // review instead of guessing.
                        self.mark_pending_unconfirmed(selection, scope, nonce).await;
                        self.publish_resolution(
                            ui,
                            selection.clone(),
                            scope.clone(),
                            None,
                            draft_recovery::update_notice(QueuedSendUpdate::Unknown, None),
                        );
                        return true;
                    }
                }
                let resolution = self.drafts.resolve_pending(
                    account,
                    room,
                    thread,
                    nonce,
                    Some(transaction_id),
                    sent,
                );
                self.persist_scope_and_index(selection, scope).await;
                (true, resolution)
            }
            QueuedSendUpdate::Retained { recoverable } => {
                self.drafts.set_pending_status(
                    account,
                    room,
                    thread,
                    nonce,
                    PendingStatus::Paused { recoverable },
                );
                (false, None)
            }
            QueuedSendUpdate::Unknown => {
                self.mark_pending_unconfirmed(selection, scope, nonce).await;
                (true, None)
            }
        };
        let notice = draft_recovery::update_notice(update, resolution.as_ref());
        self.publish_resolution(ui, selection.clone(), scope.clone(), resolution, notice);
        terminal
    }

    /// Cancel exactly the request the user clicked (captured target), under
    /// the selection they clicked in. The SDK reports whether it could still
    /// abort; a message that already reached the server stays sent.
    pub(crate) async fn cancel_pending_send(
        self: &Arc<Self>,
        selection: AccountSelection,
        scope: SendScope,
        target: PendingTarget,
        ui: Weak<AppWindow>,
    ) {
        let notice = match self
            .manager
            .cancel_pending_send_for(
                &selection,
                &scope.room,
                scope.thread(),
                &target.nonce,
                &target.transaction_id,
            )
            .await
        {
            Ok(true) => "Cancelling. A message that already reached the server stays sent.",
            Ok(false) => "This message was already sent and cannot be cancelled.",
            Err(MatrixCoreError::PendingSendUnavailable) if selection.is_current() => {
                self.mark_pending_unconfirmed(&selection, &scope, &target.nonce)
                    .await;
                "This message is no longer waiting here. Check the conversation, then choose."
            }
            Err(MatrixCoreError::SelectedAccountChanged) => return,
            Err(MatrixCoreError::RecoveryIncomplete) => {
                "Saved send status for this room is still being checked. Try again in a moment."
            }
            Err(_) => "Could not cancel right now. The message stays queued.",
        };
        self.publish_resolution(&ui, selection, scope, None, notice);
    }

    /// Resume exactly the paused request the user clicked. No new request is
    /// created.
    pub(crate) async fn retry_pending_send(
        self: &Arc<Self>,
        selection: AccountSelection,
        scope: SendScope,
        target: PendingTarget,
        ui: Weak<AppWindow>,
    ) {
        let notice = match self
            .manager
            .retry_pending_send_for(
                &selection,
                &scope.room,
                scope.thread(),
                &target.nonce,
                &target.transaction_id,
            )
            .await
        {
            Ok(()) if selection.is_current() => {
                self.drafts.set_pending_status(
                    &scope.account,
                    &scope.room,
                    scope.thread(),
                    &target.nonce,
                    PendingStatus::Queued,
                );
                "Retrying."
            }
            Ok(()) => return,
            Err(MatrixCoreError::PendingSendUnavailable) if selection.is_current() => {
                self.mark_pending_unconfirmed(&selection, &scope, &target.nonce)
                    .await;
                "This message is no longer waiting here. Check the conversation, then choose."
            }
            Err(MatrixCoreError::SelectedAccountChanged) => return,
            Err(MatrixCoreError::RecoveryIncomplete) => {
                "Saved send status for this room is still being checked. Try again in a moment."
            }
            Err(_) => "Could not retry right now. The message stays queued.",
        };
        self.publish_resolution(&ui, selection, scope, None, notice);
    }

    /// The user checked the conversation and decided for exactly one
    /// unconfirmed record. `sent` drops it with its text; otherwise its text
    /// returns to the empty composer or is kept beside newer text. Nothing is
    /// sent either way.
    pub(crate) async fn review_unconfirmed_send(
        self: &Arc<Self>,
        selection: AccountSelection,
        scope: SendScope,
        nonce: Generation,
        sent: bool,
        ui: Weak<AppWindow>,
    ) {
        if !selection.is_current()
            || self
                .drafts
                .record_status(&scope.account, &scope.room, scope.thread(), &nonce)
                != Some(PendingStatus::Unconfirmed)
        {
            return;
        }
        if !self.release_record(&selection, &scope, &nonce).await {
            self.publish_resolution(
                &ui,
                selection,
                scope,
                None,
                "Could not update this message's saved status right now.",
            );
            return;
        }
        if !selection.is_current() {
            return;
        }
        let resolution = self.drafts.review_unconfirmed(
            &scope.account,
            &scope.room,
            scope.thread(),
            &nonce,
            sent,
        );
        if resolution.is_none() {
            return;
        }
        self.persist_scope_and_index(&selection, &scope).await;
        let notice = match &resolution {
            _ if sent => "Marked as sent. Nothing was resent.",
            Some(resolution) if resolution.restored.is_some() => {
                "Marked as not sent; its text is back in the composer. Nothing was resent."
            }
            _ => "Marked as not sent; its text is kept below the composer. Nothing was resent.",
        };
        self.publish_resolution(&ui, selection, scope, resolution, notice);
    }

    /// Put exactly one record's text into the composer the user was looking
    /// at, provided it is still empty at the same edit revision. Kept text is
    /// consumed (released in the core first); an awaiting record keeps its
    /// copy because its message may still send.
    pub(crate) async fn restore_record_text(
        self: &Arc<Self>,
        selection: AccountSelection,
        scope: SendScope,
        nonce: Generation,
        expected_revision: u64,
        ui: Weak<AppWindow>,
    ) {
        if !selection.is_current() || selection.account_id() != scope.account {
            return;
        }
        let kept = self
            .drafts
            .record_status(&scope.account, &scope.room, scope.thread(), &nonce)
            == Some(PendingStatus::Kept);
        if kept && !self.release_record(&selection, &scope, &nonce).await {
            self.publish_resolution(
                &ui,
                selection,
                scope,
                None,
                "Could not update this message's saved status right now.",
            );
            return;
        }
        // The cache is mutated only under the original selection; a delayed
        // click after a switch (or A->B->A) changes nothing.
        if !selection.is_current() {
            return;
        }
        let resolution = self.drafts.restore_record_text(
            &scope.account,
            &scope.room,
            scope.thread(),
            &nonce,
            expected_revision,
        );
        let notice = match &resolution {
            Some(_) if kept => "Kept text restored to the composer.",
            Some(_) => "Text restored to the composer. The queued message may still send.",
            None => "The composer changed; nothing was restored.",
        };
        if resolution.is_some() {
            self.persist_scope_and_index(&selection, &scope).await;
        }
        self.publish_resolution(&ui, selection, scope, resolution, notice);
    }

    /// Explicitly discard exactly one kept text.
    pub(crate) async fn discard_kept_text(
        self: &Arc<Self>,
        selection: AccountSelection,
        scope: SendScope,
        nonce: Generation,
        ui: Weak<AppWindow>,
    ) {
        if !selection.is_current()
            || self
                .drafts
                .record_status(&scope.account, &scope.room, scope.thread(), &nonce)
                != Some(PendingStatus::Kept)
            || !self.release_record(&selection, &scope, &nonce).await
            || !selection.is_current()
        {
            return;
        }
        if self
            .drafts
            .discard_kept(&scope.account, &scope.room, scope.thread(), &nonce)
        {
            self.persist_scope_and_index(&selection, &scope).await;
        }
        self.publish_resolution(&ui, selection, scope, None, "Kept text discarded.");
    }

    /// Explicitly discard saved data this version could not read in one
    /// scope. The core releases the row first; the next save replaces it.
    pub(crate) async fn discard_unreadable_status(
        self: &Arc<Self>,
        selection: AccountSelection,
        scope: SendScope,
        ui: Weak<AppWindow>,
    ) {
        // The discard applies to exactly the original the user reviewed: the
        // core compares the complete stored row (both text columns, kind and
        // every attachment value) against that original before releasing it,
        // and the cache checks the same identity again.
        let Some(revision) =
            self.drafts
                .quarantine_revision(&scope.account, &scope.room, scope.thread())
        else {
            return;
        };
        if !draft_recovery::still_owned(&selection, &scope) {
            return;
        }
        let discarded = self
            .manager
            .discard_quarantine_for(&selection, &scope.room, scope.thread(), &revision)
            .await;
        if !draft_recovery::still_owned(&selection, &scope) {
            return;
        }
        let notice = match discarded {
            Ok(()) => {
                if self.drafts.discard_quarantine(
                    &scope.account,
                    &scope.room,
                    scope.thread(),
                    &revision,
                ) {
                    self.persist_scope_and_index(&selection, &scope).await;
                }
                "Unreadable saved status discarded. Sending here is available again."
            }
            Err(MatrixCoreError::SelectedAccountChanged) => return,
            Err(MatrixCoreError::StaleDraftSnapshot) => {
                "Saved data here changed since you reviewed it. It is kept untouched; review it again before discarding."
            }
            Err(MatrixCoreError::OriginalUnverifiable) => {
                "Saved data here is too large for this version to verify before discarding. It is kept untouched."
            }
            Err(_) => "Could not discard the saved data right now. It is kept untouched.",
        };
        self.publish_resolution(&ui, selection, scope, None, notice);
    }

    /// Seed the cache with every draft scope the core found at selection and
    /// start exact observers for the requests the SDK still holds. The epoch
    /// is read before waiting on the core; a removal or switch in between
    /// leaves the erased cache alone and starts no observer.
    pub(crate) async fn adopt_recovered_sends(
        self: &Arc<Self>,
        selection: AccountSelection,
        ui: Weak<AppWindow>,
    ) {
        let epoch = self.drafts.account_epoch(selection.account_id());
        let Ok(recovered) = self.manager.take_recovered_drafts(&selection).await else {
            return;
        };
        self.seed_and_observe(&selection, epoch, recovered, &ui);
    }

    /// A room the selection-time discovery could not cover is scanned when
    /// opened; until then its send queue stays paused and sends are refused.
    pub(crate) async fn complete_room_recovery(
        self: &Arc<Self>,
        selection: AccountSelection,
        room_id: String,
        ui: Weak<AppWindow>,
    ) {
        let epoch = self.drafts.account_epoch(selection.account_id());
        match self
            .manager
            .room_recovery_incomplete(&selection, &room_id)
            .await
        {
            Ok(true) => {}
            _ => return,
        }
        let notice = match self.manager.recover_room_for(&selection, &room_id).await {
            Ok(recovered) => {
                self.seed_and_observe(&selection, epoch, recovered, &ui);
                ""
            }
            Err(MatrixCoreError::SelectedAccountChanged) => return,
            Err(_) => {
                "Saved send status for this room could not be read; sending here stays paused until it can be."
            }
        };
        let scope = SendScope {
            account: selection.account_id().to_owned(),
            room: room_id,
            thread: None,
        };
        self.publish_resolution(&ui, selection, scope, None, notice);
    }

    fn seed_and_observe(
        self: &Arc<Self>,
        selection: &AccountSelection,
        epoch: u64,
        recovered: Vec<RecoveredDraft>,
        ui: &Weak<AppWindow>,
    ) {
        let account = selection.account_id().to_owned();
        for draft in recovered {
            if !selection.is_current() || self.drafts.account_epoch(&account) != epoch {
                return;
            }
            let attached: Vec<Generation> = draft
                .sends
                .iter()
                .filter(|send| send.queued.is_some())
                .map(|send| send.nonce.clone())
                .collect();
            let scope = SendScope {
                account: account.clone(),
                room: draft.room_id,
                thread: draft.thread_root,
            };
            self.drafts.seed_recovered(
                &scope.account,
                &scope.room,
                scope.thread(),
                draft.restored,
                &attached,
                epoch,
            );
            for send in draft.sends {
                if let (Some(queued), Some(transaction_id)) = (send.queued, send.transaction_id) {
                    draft_recovery::spawn_observer(
                        self.clone(),
                        ui.clone(),
                        selection.clone(),
                        scope.clone(),
                        send.nonce,
                        transaction_id,
                        queued,
                    );
                }
            }
        }
        let live = self.clone();
        let selection = selection.clone();
        let _ = ui.upgrade_in_event_loop(move |window| {
            if !selection.is_current() {
                return;
            }
            for threaded in [false, true] {
                if let Some(scope) = draft_recovery::current_scope(&window, threaded)
                    && scope.account == selection.account_id()
                {
                    draft_recovery::publish_scope(&window, &live, &scope);
                }
            }
        });
    }

    pub fn forget_account_drafts(&self, account_id: &str) {
        self.drafts.forget_account(account_id);
    }

    pub async fn flush_drafts(&self) -> Result<(), MatrixCoreError> {
        self.drafts.flush_dirty(&self.manager).await
    }

    fn queue_draft_write(&self) {
        // One queued signal is enough: the worker scans the latest dirty
        // cache, coalescing fast typing without creating a task per keypress.
        let _ = self.draft_write_tx.try_send(());
    }

    pub async fn restore(self: &Arc<Self>, ui: Weak<AppWindow>) {
        match self.manager.restore_last_active().await {
            Ok(Some(account)) => {
                self.is_live.store(true, Ordering::Release);
                if let Some(selection) = self.manager.account_selection(&account.id) {
                    self.adopt_recovered_sends(selection, ui.clone()).await;
                }
                if let Err(error) = self.refresh(ui.clone()).await {
                    crate::show_login_error(&ui, error.to_string());
                } else {
                    self.clone().scan_spaces(ui).await;
                }
            }
            Ok(None) => {
                let _ = ui.upgrade_in_event_loop(|ui| ui.set_login_loading(false));
            }
            Err(error) => crate::show_login_error(&ui, format!("Sign in again: {error}")),
        }
    }

    /// Complete a core-authorized account activation while holding account_transition.
    pub(crate) async fn account_activated(
        self: &Arc<Self>,
        ui: Weak<AppWindow>,
    ) -> Result<(), MatrixCoreError> {
        self.invalidate_navigation();
        self.clear_feed().await;
        *self.selected_room.lock().await = None;
        *self.selected_space.lock().await = None;
        self.invalidate_space_scan().await;
        self.space_children.lock().await.clear();
        self.is_live.store(true, Ordering::Release);
        if let Some(selection) = self
            .manager
            .active_account()
            .await
            .and_then(|account| self.manager.account_selection(&account.id))
        {
            self.adopt_recovered_sends(selection, ui.clone()).await;
        }
        if let Err(error) = self.refresh(ui.clone()).await {
            self.manager.stop().await;
            self.account_detached(ui).await;
            return Err(error);
        }
        let _ = ui.upgrade_in_event_loop(|ui| {
            ui.set_login_loading(false);
            ui.set_login_error("".into());
            ui.set_show_sign_in(false);
        });
        tokio::spawn(self.clone().scan_spaces(ui));
        Ok(())
    }

    /// Hide revoked account data without automatically activating another account.
    pub(crate) async fn account_detached(self: &Arc<Self>, ui: Weak<AppWindow>) {
        self.is_live.store(false, Ordering::Release);
        self.invalidate_navigation();
        self.render_epoch.fetch_add(1, Ordering::AcqRel);
        self.clear_feed().await;
        *self.selected_room.lock().await = None;
        *self.selected_space.lock().await = None;
        self.invalidate_space_scan().await;
        self.space_children.lock().await.clear();
        let gate = self.security_gate.clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            ui.invoke_cancel_room_settings();
            ui.invoke_close_image_preview();
            crate::reset_room_search(&ui);
            crate::reset_attachment_view(&ui);
            clear_account_context(&ui, &gate);
            ui.set_live_session(false);
            ui.set_preview_session(false);
            ui.set_active_account_local_id("".into());
            // No account is active: show the global theme, never a departed
            // account's override.
            crate::appearance::apply_theme_mode(&ui);
            ui.set_active_account_index(-1);
            ui.set_active_account_name("Choose an account".into());
            ui.set_active_account_handle("".into());
            ui.set_active_account_initials("".into());
            ui.set_active_account_status("Locked".into());
            ui.set_account_options(ModelRc::new(VecModel::<AccountOption>::default()));
            ui.set_accounts(ModelRc::new(VecModel::<AccountItem>::default()));
            ui.set_active_room_id("".into());
            ui.set_active_room_name("".into());
            ui.set_active_room_topic("".into());
            ui.set_active_space_id("".into());
            ui.set_active_space_name("Home".into());
            ui.set_spaces(ModelRc::new(VecModel::<SpaceItem>::default()));
            ui.set_channels(ModelRc::new(VecModel::<RoomItem>::default()));
            ui.set_direct_messages(ModelRc::new(VecModel::<RoomItem>::default()));
            ui.set_combined_rooms(ModelRc::new(
                VecModel::<crate::NavigationRoomItem>::default(),
            ));
            ui.set_unsorted_channels(ModelRc::new(VecModel::<RoomItem>::default()));
            ui.set_unsorted_direct_messages(ModelRc::new(VecModel::<RoomItem>::default()));
            ui.set_messages(ModelRc::new(VecModel::<MessageItem>::default()));
            ui.set_thread_messages(ModelRc::new(VecModel::<MessageItem>::default()));
            ui.set_members(ModelRc::new(VecModel::<MemberItem>::default()));
            ui.set_composer_text("".into());
            ui.set_composer_saved_draft("".into());
            ui.set_composer_mode(0);
            ui.set_composer_target_id("".into());
            ui.set_composer_target_label("".into());
            ui.set_composer_busy(false);
            ui.set_thread_root_id("".into());
            ui.set_thread_root_sender("".into());
            ui.set_thread_root_is_own(false);
            ui.set_thread_root_palette_index(0);
            ui.set_thread_root_accent(Color::default());
            ui.set_thread_root_body("".into());
            ui.set_thread_root_rich_body(Default::default());
            ui.set_thread_root_rich_accessible_body("".into());
            ui.set_thread_composer_text("".into());
            draft_recovery::clear_recovery_ui(&ui);
            ui.set_room_actions_open(false);
            ui.set_member_confirm_open(false);
            ui.set_quick_switcher_open(false);
            ui.set_show_sign_in(true);
            ui.set_login_loading(false);
            ui.set_session_error("".into());
        });
    }

    pub async fn select_account(
        self: &Arc<Self>,
        index: usize,
        ui: Weak<AppWindow>,
    ) -> Result<(), MatrixCoreError> {
        self.select_account_guarded(index, ui, || true).await?;
        Ok(())
    }

    /// Check the call switch gate while holding the same transition lock that
    /// call entry uses. A check before awaiting this lock can become stale.
    pub async fn select_account_guarded(
        self: &Arc<Self>,
        index: usize,
        ui: Weak<AppWindow>,
        can_switch: impl FnOnce() -> bool,
    ) -> Result<bool, MatrixCoreError> {
        let _transition = self.account_transition.lock().await;
        if !can_switch() {
            return Ok(false);
        }
        self.invalidate_navigation();
        let accounts = self.manager.accounts().await;
        let account = accounts
            .get(index)
            .ok_or_else(|| MatrixCoreError::AccountNotFound(index.to_string()))?;
        self.flush_drafts().await?;
        self.clear_feed().await;
        self.manager.switch_to(&account.id).await?;
        *self.selected_room.lock().await = None;
        *self.selected_space.lock().await = None;
        self.invalidate_space_scan().await;
        self.space_children.lock().await.clear();
        self.is_live.store(true, Ordering::Release);
        if let Some(selection) = self.manager.account_selection(&account.id) {
            self.adopt_recovered_sends(selection, ui.clone()).await;
        }
        self.refresh(ui.clone()).await?;
        tokio::spawn(self.clone().scan_spaces(ui));
        Ok(true)
    }

    /// Keep an action tied to the account visible when it was submitted. The
    /// same guard covers account switches, login, and logout in the app.
    pub async fn run_room_action(
        self: &Arc<Self>,
        account_id: &str,
        action: RoomAction,
        input: &str,
        ui: Weak<AppWindow>,
    ) -> Result<(), MatrixCoreError> {
        let _transition = self.account_transition.lock().await;
        self.require_active_account(account_id).await?;
        let input = input.trim();
        if input.is_empty() {
            return Err(MatrixCoreError::InvalidRegistry(
                "Enter a room or Space name".into(),
            ));
        }
        let room_id = match action {
            RoomAction::CreatePrivate => self.manager.create_private_room(input).await?,
            RoomAction::CreateSpace => self.manager.create_space(input).await?,
        };
        self.complete_room_action(
            account_id,
            room_id,
            Some(matches!(action, RoomAction::CreateSpace)),
            input,
            ui,
        )
        .await
    }

    /// Join a parsed room ID, alias, or Matrix link and navigate to it. The
    /// directory may already know this is a Space before local sync does.
    pub async fn join_matrix_target(
        self: &Arc<Self>,
        account_id: &str,
        target: &MatrixLinkTarget,
        space_hint: Option<bool>,
        display_name: &str,
        ui: Weak<AppWindow>,
    ) -> Result<(), MatrixCoreError> {
        let _transition = self.account_transition.lock().await;
        self.require_active_account(account_id).await?;
        let room_id = self
            .manager
            .join_matrix_room_for(account_id, target)
            .await?;
        self.complete_room_action(account_id, room_id, space_hint, display_name, ui)
            .await
    }

    async fn complete_room_action(
        self: &Arc<Self>,
        account_id: &str,
        room_id: String,
        space_hint: Option<bool>,
        display_name: &str,
        ui: Weak<AppWindow>,
    ) -> Result<(), MatrixCoreError> {
        self.require_active_account(account_id).await?;
        let is_space = match space_hint {
            Some(true) => true,
            Some(false) | None => self
                .manager
                .rooms_snapshot()
                .await?
                .iter()
                .any(|room| room.room_id == room_id && room.is_space),
        };
        if is_space {
            self.known_spaces
                .lock()
                .await
                .entry(account_id.to_owned())
                .or_default()
                .insert(
                    room_id.clone(),
                    KnownSpace {
                        name: if display_name.is_empty() {
                            room_id.clone()
                        } else {
                            display_name.to_owned()
                        },
                        recorded_at: Instant::now(),
                    },
                );
        }
        self.clear_feed().await;
        if is_space {
            *self.selected_space.lock().await = Some(room_id);
            *self.selected_room.lock().await = None;
        } else {
            *self.selected_space.lock().await = None;
            *self.selected_room.lock().await = Some(room_id);
        }
        self.invalidate_space_scan().await;
        self.invalidate_navigation();
        self.refresh(ui.clone()).await?;
        tokio::spawn(self.clone().scan_spaces(ui));
        Ok(())
    }

    pub async fn respond_to_invite(
        self: &Arc<Self>,
        account_id: &str,
        room_id: &str,
        accept: bool,
        ui: Weak<AppWindow>,
    ) -> Result<(), MatrixCoreError> {
        let _transition = self.account_transition.lock().await;
        self.require_active_account(account_id).await?;
        if accept {
            // Invited Spaces carry the room type in their stripped state.
            // Check before joining: a newly joined room may not yet have its
            // full create event in the local room snapshot.
            let invite = self
                .manager
                .invites_snapshot()
                .await?
                .into_iter()
                .find(|room| room.room_id == room_id);
            let was_space = invite.as_ref().is_some_and(|room| room.is_space);
            self.manager.accept_invite(room_id).await?;
            let is_space = was_space
                || self
                    .manager
                    .rooms_snapshot()
                    .await?
                    .iter()
                    .any(|room| room.room_id == room_id && room.is_space);
            if is_space {
                self.known_spaces
                    .lock()
                    .await
                    .entry(account_id.to_owned())
                    .or_default()
                    .insert(
                        room_id.to_owned(),
                        KnownSpace {
                            name: invite
                                .as_ref()
                                .map_or_else(|| room_id.to_owned(), |room| room.name.clone()),
                            recorded_at: Instant::now(),
                        },
                    );
            }
            self.clear_feed().await;
            if is_space {
                *self.selected_space.lock().await = Some(room_id.to_owned());
                *self.selected_room.lock().await = None;
            } else {
                *self.selected_space.lock().await = None;
                *self.selected_room.lock().await = Some(room_id.to_owned());
            }
        } else {
            self.manager.reject_invite(room_id).await?;
        }
        self.require_active_account(account_id).await?;
        self.invalidate_space_scan().await;
        self.invalidate_navigation();
        self.refresh(ui.clone()).await?;
        tokio::spawn(self.clone().scan_spaces(ui));
        Ok(())
    }

    async fn require_active_account(&self, account_id: &str) -> Result<(), MatrixCoreError> {
        if self
            .manager
            .active_account()
            .await
            .as_ref()
            .is_none_or(|active| active.id != account_id)
        {
            Err(MatrixCoreError::SelectedAccountChanged)
        } else {
            Ok(())
        }
    }

    pub async fn select_room_for(
        self: &Arc<Self>,
        account_id: &str,
        room_id: String,
        navigation_epoch: u64,
        ui: Weak<AppWindow>,
    ) -> Result<bool, MatrixCoreError> {
        let _transition = self.account_transition.lock().await;
        if !self.navigation_is_current(navigation_epoch)
            || self.require_active_account(account_id).await.is_err()
        {
            return Ok(false);
        }
        self.clear_feed().await;
        {
            let mut selected = self.selected_room.lock().await;
            if !self.navigation_is_current(navigation_epoch) {
                return Ok(false);
            }
            *selected = Some(room_id);
        }
        if !self.navigation_is_current(navigation_epoch) {
            return Ok(false);
        }
        self.refresh(ui.clone()).await?;
        if let Some(selection) = self.manager.account_selection(account_id) {
            let room_id = self.selected_room.lock().await.clone();
            if let Some(room_id) = room_id {
                self.complete_room_recovery(selection, room_id, ui).await;
            }
        }
        Ok(true)
    }

    pub async fn select_space_for(
        self: &Arc<Self>,
        account_id: &str,
        space_id: String,
        navigation_epoch: u64,
        ui: Weak<AppWindow>,
    ) -> Result<bool, MatrixCoreError> {
        let _transition = self.account_transition.lock().await;
        if !self.navigation_is_current(navigation_epoch)
            || self.require_active_account(account_id).await.is_err()
        {
            return Ok(false);
        }
        if !space_id.is_empty() {
            let rooms = self.manager.rooms_snapshot().await?;
            let known = self
                .known_spaces
                .lock()
                .await
                .get(account_id)
                .is_some_and(|spaces| spaces.contains_key(&space_id));
            if !known
                && !rooms
                    .iter()
                    .any(|room| room.is_space && room.room_id == space_id)
            {
                return Err(MatrixCoreError::RoomNotFound(space_id));
            }
        }
        if !self.navigation_is_current(navigation_epoch) {
            return Ok(false);
        }
        *self.selected_space.lock().await = (!space_id.is_empty()).then_some(space_id);
        *self.selected_room.lock().await = None;
        self.clear_feed().await;
        if !self.navigation_is_current(navigation_epoch) {
            return Ok(false);
        }
        self.refresh(ui).await?;
        Ok(true)
    }

    pub(crate) fn navigation_is_current(&self, navigation_epoch: u64) -> bool {
        self.is_live.load(Ordering::Acquire)
            && self.navigation_epoch.load(Ordering::Acquire) == navigation_epoch
    }

    /// Refresh saved identities after removal without reopening the room or
    /// querying its members. A queued pre-removal snapshot must not reinsert it.
    pub(crate) async fn refresh_account_menu(&self, ui: Weak<AppWindow>) {
        let render_epoch = self.render_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let accounts = self.manager.accounts().await;
        let Some(active) = self.manager.active_account().await else {
            return;
        };
        let current_epoch = self.render_epoch.clone();
        let is_live = self.is_live.clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            if is_live.load(Ordering::Acquire)
                && current_epoch.load(Ordering::Acquire) == render_epoch
                && ui.get_active_account_local_id().as_str() == active.id
            {
                apply_account_menu(&ui, &accounts, &active);
            }
        });
    }

    pub async fn refresh(self: &Arc<Self>, ui: Weak<AppWindow>) -> Result<(), MatrixCoreError> {
        if !self.is_live.load(Ordering::Acquire) {
            return Ok(());
        }
        let render_epoch = self.render_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let navigation_epoch = self.navigation_epoch.load(Ordering::Acquire);
        let accounts = self.manager.accounts().await;
        let active = self
            .manager
            .active_account()
            .await
            .ok_or(MatrixCoreError::NoActiveAccount)?;
        let rooms = self.manager.rooms_snapshot().await?;
        if self
            .manager
            .active_account()
            .await
            .as_ref()
            .map(|account| &account.id)
            != Some(&active.id)
            || self.render_epoch.load(Ordering::Acquire) != render_epoch
        {
            return Ok(());
        }
        let known_spaces = {
            let mut by_account = self.known_spaces.lock().await;
            let known = by_account.entry(active.id.clone()).or_default();
            known.retain(
                |id, value| match rooms.iter().find(|room| room.room_id == *id) {
                    Some(room) if room.is_space && room.membership == RoomMembership::Joined => {
                        false
                    }
                    _ => value.recorded_at.elapsed() < Duration::from_secs(60),
                },
            );
            known
                .iter()
                .map(|(id, value)| (id.clone(), value.name.clone()))
                .collect::<HashMap<_, _>>()
        };
        let selected_space = self.selected_space.lock().await.clone().filter(|space_id| {
            known_spaces.contains_key(space_id)
                || rooms.iter().any(|room| {
                    room.is_space
                        && room.room_id == *space_id
                        && room.membership == RoomMembership::Joined
                })
        });
        if self.render_epoch.load(Ordering::Acquire) != render_epoch {
            return Ok(());
        }
        *self.selected_space.lock().await = selected_space.clone();
        let space_children = self.space_children.lock().await.clone();
        let all_space_children = space_children
            .values()
            .flatten()
            .cloned()
            .collect::<HashSet<_>>();
        let visible = |room: &RoomInfo| {
            if room.is_space
                || known_spaces.contains_key(&room.room_id)
                || room.membership != RoomMembership::Joined
            {
                return false;
            }
            if let Some(space_id) = selected_space.as_deref() {
                space_children
                    .get(space_id)
                    .is_some_and(|children| children.contains(&room.room_id))
            } else {
                room.is_direct || !all_space_children.contains(&room.room_id)
            }
        };
        let current = self.selected_room.lock().await.clone();
        let selected = current
            .filter(|room_id| {
                rooms
                    .iter()
                    .any(|room| room.room_id == *room_id && visible(room))
            })
            .or_else(|| {
                rooms
                    .iter()
                    .find(|room| room.membership == RoomMembership::Joined && visible(room))
                    .map(|room| room.room_id.clone())
            });
        if self.render_epoch.load(Ordering::Acquire) != render_epoch {
            return Ok(());
        }
        *self.selected_room.lock().await = selected.clone();
        if let Some(room_id) = selected.as_deref() {
            if let Err(error) = self
                .drafts
                .load_if_missing(&self.manager, &active.id, room_id)
                .await
            {
                show_error(
                    &ui,
                    format!("Could not restore draft on this device: {error}"),
                );
            }
        }
        if self.render_epoch.load(Ordering::Acquire) != render_epoch {
            return Ok(());
        }
        let member_account_id = active.id.clone();
        let marker_rooms = rooms
            .iter()
            .filter(|room| room.membership == RoomMembership::Joined && !room.is_space)
            .map(|room| room.room_id.clone())
            .collect::<Vec<_>>();
        let view = Snapshot {
            accounts,
            active,
            rooms,
            selected: selected.clone(),
            selected_space,
            known_spaces,
            space_children,
            all_space_children,
            local_voice_rooms: self.voice_spaces.local_rooms(&member_account_id),
            shared_voice_rooms: self.voice_spaces.shared_rooms(&member_account_id).await,
        };
        let current_epoch = self.render_epoch.clone();
        let is_live = self.is_live.clone();
        let drafts = self.drafts.clone();
        let security_gate = self.security_gate.clone();
        let voice_spaces = self.voice_spaces.clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            if is_live.load(Ordering::Acquire)
                && current_epoch.load(Ordering::Acquire) == render_epoch
            {
                apply_snapshot(&ui, view, &drafts, &security_gate, &voice_spaces);
            }
        });
        if let Some(room_id) = selected {
            self.refresh_members_for(
                member_account_id.clone(),
                room_id.clone(),
                ui.clone(),
                false,
            )
            .await;
            self.open_room(&member_account_id, room_id, navigation_epoch, ui.clone())
                .await;
        }
        let marker_account = member_account_id;
        let voice_spaces = self.voice_spaces.clone();
        let live = self.clone();
        tokio::spawn(async move {
            if voice_spaces
                .scan_shared(&live.manager, &marker_account, &marker_rooms)
                .await
            {
                let shared_rooms = voice_spaces.shared_rooms(&marker_account).await;
                let _ = ui.upgrade_in_event_loop(move |window| {
                    if window.get_active_account_local_id().as_str() == marker_account {
                        apply_shared_voice_labels(&window, &shared_rooms);
                    }
                });
            }
        });
        Ok(())
    }

    /// Fetch members for the visible account and room. Sync can trigger many
    /// presentation refreshes, so refresh at most once per short interval
    /// unless the user explicitly requests it.
    pub async fn refresh_members_for(
        self: &Arc<Self>,
        account_id: String,
        room_id: String,
        ui: Weak<AppWindow>,
        force: bool,
    ) {
        if !self.is_live.load(Ordering::Acquire)
            || self.selected_room.lock().await.as_deref() != Some(room_id.as_str())
            || self
                .manager
                .active_account()
                .await
                .is_none_or(|account| account.id != account_id)
        {
            return;
        }
        let epoch = {
            let mut request = self.member_request.lock().await;
            if !force
                && request.as_ref().is_some_and(|previous| {
                    previous.account_id == account_id
                        && previous.room_id == room_id
                        && previous.epoch == self.member_epoch.load(Ordering::Acquire)
                        && previous.started.elapsed() < Duration::from_secs(30)
                })
            {
                return;
            }
            let epoch = self.member_epoch.fetch_add(1, Ordering::AcqRel) + 1;
            *request = Some(MemberRequest {
                account_id: account_id.clone(),
                room_id: room_id.clone(),
                started: Instant::now(),
                epoch,
            });
            epoch
        };
        let loading_account = account_id.clone();
        let loading_room = room_id.clone();
        let _ = ui.upgrade_in_event_loop(move |window| {
            if window.get_active_account_local_id().as_str() == loading_account
                && window.get_active_room_id().as_str() == loading_room
            {
                window.set_member_loading(true);
                window.set_member_error("".into());
            }
        });
        let live = self.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(30),
                live.manager.room_members_for(&account_id, &room_id),
            )
            .await;
            if !live.is_live.load(Ordering::Acquire)
                || live.member_epoch.load(Ordering::Acquire) != epoch
                || live.selected_room.lock().await.as_deref() != Some(room_id.as_str())
                || live
                    .manager
                    .active_account()
                    .await
                    .is_none_or(|account| account.id != account_id)
            {
                return;
            }
            let (items, error) = match result {
                Ok(Ok(members)) => (
                    Some(members.iter().map(member_item).collect::<Vec<_>>()),
                    None,
                ),
                Ok(Err(error)) => (
                    None,
                    Some(crate::directory_plain_text(
                        &format!("Could not load members: {error}"),
                        240,
                    )),
                ),
                Err(_) => (
                    None,
                    Some("Loading members timed out. Retry to refresh.".to_owned()),
                ),
            };
            let _ = ui.upgrade_in_event_loop(move |window| {
                if live.member_epoch.load(Ordering::Acquire) != epoch
                    || window.get_active_account_local_id().as_str() != account_id
                    || window.get_active_room_id().as_str() != room_id
                {
                    return;
                }
                window.set_member_loading(false);
                if let Some(items) = items {
                    window.set_members(ModelRc::new(VecModel::from(items)));
                    window.set_member_error("".into());
                } else if let Some(error) = error {
                    window.set_member_error(error.into());
                }
            });
        });
    }

    /// Load the account's Space graph after showing its cached rooms. A room
    /// may be present in several returned sets; Home excludes their union.
    pub async fn scan_spaces(self: Arc<Self>, ui: Weak<AppWindow>) {
        if !self.is_live.load(Ordering::Acquire) {
            return;
        }
        let Some(account) = self.manager.active_account().await else {
            return;
        };
        let rooms = match self.manager.rooms_snapshot().await {
            Ok(rooms) => rooms,
            Err(error) => {
                show_error(&ui, error.to_string());
                return;
            }
        };
        let known_ids = self
            .known_spaces
            .lock()
            .await
            .get(&account.id)
            .map(|spaces| spaces.keys().cloned().collect::<HashSet<_>>())
            .unwrap_or_default();
        let mut space_ids = rooms
            .iter()
            .filter(|room| {
                (room.is_space || known_ids.contains(&room.room_id))
                    && room.membership == RoomMembership::Joined
            })
            .map(|room| room.room_id.clone())
            .collect::<Vec<_>>();
        space_ids.sort();
        let key = (account.id.clone(), space_ids.clone());
        {
            let mut scanned = self.space_scan_key.lock().await;
            if scanned.as_ref() == Some(&key) {
                let failure = *self.space_scan_failed_at.lock().await;
                if failure.is_none_or(|at| at.elapsed() < Duration::from_secs(30)) {
                    return;
                }
            }
            *scanned = Some(key);
        }
        let scan_epoch = self.space_scan_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        *self.space_scan_failed_at.lock().await = None;
        let mut graph = HashMap::new();
        let mut failed = false;
        for space_id in space_ids {
            if self
                .manager
                .active_account()
                .await
                .is_none_or(|active| active.id != account.id)
            {
                return;
            }
            let mut descendants = HashSet::new();
            let mut from = None;
            for _ in 0..20 {
                let result = tokio::time::timeout(
                    Duration::from_secs(10),
                    self.manager
                        .space_hierarchy(&space_id, from.as_deref(), 100),
                )
                .await;
                let page = match result {
                    Ok(Ok(page)) => page,
                    Ok(Err(error)) => {
                        show_error(&ui, format!("Space {}: {error}", space_id));
                        failed = true;
                        break;
                    }
                    Err(_) => {
                        show_error(&ui, format!("Space {} timed out", space_id));
                        failed = true;
                        break;
                    }
                };
                for node in page.nodes {
                    if node.room_id != space_id && !node.is_space {
                        descendants.insert(node.room_id);
                    }
                }
                from = page.next_batch;
                if from.is_none() {
                    break;
                }
            }
            graph.insert(space_id, descendants);
            if failed {
                break;
            }
        }
        if self.space_scan_epoch.load(Ordering::Acquire) != scan_epoch {
            return;
        }
        if failed {
            *self.space_scan_failed_at.lock().await = Some(Instant::now());
            return;
        }
        if self
            .manager
            .active_account()
            .await
            .is_none_or(|active| active.id != account.id)
        {
            return;
        }
        *self.space_children.lock().await = graph;
        if let Err(error) = self.refresh(ui.clone()).await {
            show_error(&ui, error.to_string());
        }
    }

    async fn invalidate_space_scan(&self) {
        self.space_scan_epoch.fetch_add(1, Ordering::AcqRel);
        *self.space_scan_key.lock().await = None;
        *self.space_scan_failed_at.lock().await = None;
    }

    async fn open_room(
        self: &Arc<Self>,
        account_id: &str,
        room_id: String,
        navigation_epoch: u64,
        ui: Weak<AppWindow>,
    ) {
        let Some(selection) = self.manager.account_selection(account_id) else {
            return;
        };
        if !self.navigation_is_current(navigation_epoch)
            || self.require_active_account(account_id).await.is_err()
            || self.selected_room.lock().await.as_deref() != Some(room_id.as_str())
        {
            return;
        }
        let requested_key = (account_id.to_owned(), room_id.clone());
        {
            let key = self.feed_key.lock().await;
            let feed = self.feed.lock().await;
            if key.as_ref() == Some(&requested_key)
                && feed.as_ref().is_some_and(|task| !task.is_finished())
            {
                return;
            }
        }
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let feed = match self
            .manager
            .message_feed_selected(&selection, &room_id)
            .await
        {
            Ok(feed) => feed,
            Err(error) => {
                if selection.is_current()
                    && self.navigation_is_current(navigation_epoch)
                    && self.require_active_account(account_id).await.is_ok()
                {
                    show_error(&ui, error.to_string());
                }
                return;
            }
        };
        if !selection.is_current()
            || self.generation.load(Ordering::Acquire) != generation
            || !self.navigation_is_current(navigation_epoch)
            || self.require_active_account(account_id).await.is_err()
            || self.selected_room.lock().await.as_deref() != Some(room_id.as_str())
        {
            return;
        }
        if let Some(previous) = self.feed.lock().await.take() {
            previous.abort();
        }
        *self.feed_key.lock().await = Some(requested_key);
        let mut updates = feed.subscribe();
        let feed_generation = self.generation.clone();
        let feed_navigation_epoch = self.navigation_epoch.clone();
        let is_live = self.is_live.clone();
        let feed_account_id = account_id.to_owned();
        let feed_room_id = room_id;
        let handle = tokio::spawn(async move {
            let _feed = feed;
            let delivery = Arc::new(crate::timeline_delivery::Latest::new());
            loop {
                let messages = updates.borrow_and_update().clone();
                #[cfg(test)]
                let observed = messages.clone();
                if delivery.submit(messages) {
                    let ui_for_update = ui.clone();
                    let current_generation = feed_generation.clone();
                    let current_navigation_epoch = feed_navigation_epoch.clone();
                    let live_flag = is_live.clone();
                    let account_id = feed_account_id.clone();
                    let room_id = feed_room_id.clone();
                    let pending = delivery.clone();
                    let selected_account = selection.clone();
                    if ui_for_update
                        .upgrade_in_event_loop(move |ui| {
                            let Some(messages) = pending.take() else {
                                return;
                            };
                            if selected_account.is_current()
                                && live_flag.load(Ordering::Acquire)
                                && current_generation.load(Ordering::Acquire) == generation
                                && current_navigation_epoch.load(Ordering::Acquire)
                                    == navigation_epoch
                                && visible_feed_target(
                                    ui.get_active_account_local_id().as_str(),
                                    ui.get_active_room_id().as_str(),
                                    &account_id,
                                    &room_id,
                                )
                            {
                                ui.set_messages(crate::timeline_model::update_shared(
                                    ui.get_messages(),
                                    messages.as_ref().clone(),
                                ));
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                // Test builds only. Reached only after a successful enqueue:
                // this snapshot is held by a queued UI callback (queued just
                // above, or still pending from an earlier successful one).
                #[cfg(test)]
                timeline_controller_tests::callback_queued(
                    &feed_account_id,
                    &feed_room_id,
                    None,
                    &observed,
                );
                if !matches!(selection.guard(updates.changed()).await, Some(Ok(()))) {
                    break;
                }
            }
        });
        *self.feed.lock().await = Some(handle);
    }

    pub async fn clear_feed(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let Some(task) = self.feed.lock().await.take() {
            task.abort();
        }
        *self.feed_key.lock().await = None;
        self.clear_thread().await;
    }

    pub async fn open_thread(
        self: &Arc<Self>,
        account_id: String,
        room_id: String,
        root_event_id: String,
        generation: u64,
        ui: Weak<AppWindow>,
    ) -> Result<(), MatrixCoreError> {
        if self.thread_generation.load(Ordering::Acquire) != generation {
            return Ok(());
        }
        let selection = self
            .manager
            .account_selection(&account_id)
            .ok_or(MatrixCoreError::SelectedAccountChanged)?;
        self.require_selected_room(&room_id).await?;
        {
            let mut feed = self.thread_feed.lock().await;
            if self.thread_generation.load(Ordering::Acquire) != generation {
                return Ok(());
            }
            if let Some(previous) = feed.take() {
                previous.abort();
            }
        }
        self.drafts
            .load_scoped_if_missing(&self.manager, &account_id, &room_id, Some(&root_event_id))
            .await?;
        let restore_drafts = self.drafts.clone();
        let restore_generation = self.thread_generation.clone();
        let restore_live = self.is_live.clone();
        let restore_account = account_id.clone();
        let restore_room = room_id.clone();
        let restore_root = root_event_id.clone();
        let restore_selection = selection.clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            if restore_selection.is_current()
                && restore_live.load(Ordering::Acquire)
                && restore_generation.load(Ordering::Acquire) == generation
                && ui.get_active_account_local_id().as_str() == restore_account
                && ui.get_active_room_id().as_str() == restore_room
                && ui.get_thread_root_id().as_str() == restore_root
            {
                // Read the current cache on the UI thread: an edit after disk
                // loading but before this callback also takes precedence.
                let restored = restore_drafts.get_composed_scoped(
                    &restore_account,
                    &restore_room,
                    Some(&restore_root),
                );
                ui.set_thread_composer_text(restored.body.into());
                ui.set_thread_markdown(restored.format == TextFormat::Markdown);
                draft_recovery::publish_thread(
                    &ui,
                    &restore_drafts.pending_summary(&restore_account, &restore_room, Some(&restore_root)),
                    &restore_drafts.pending_items(&restore_account, &restore_room, Some(&restore_root)),
                );
                if restore_drafts.take_rich_recovery_notice(&restore_account, &restore_room, Some(&restore_root)) {
                    ui.set_transfer_notice("Draft text restored; unsupported saved formatting and mention selections were discarded.".into());
                }
            }
        });
        if self.thread_generation.load(Ordering::Acquire) != generation {
            return Ok(());
        }
        let feed = self
            .manager
            .thread_message_feed_selected(&selection, &room_id, &root_event_id)
            .await?;
        if self.thread_generation.load(Ordering::Acquire) != generation {
            return Ok(());
        }
        let mut updates = feed.subscribe();
        let current_generation = self.thread_generation.clone();
        let is_live = self.is_live.clone();
        let handle = tokio::spawn(async move {
            let _feed = feed;
            let delivery = Arc::new(crate::timeline_delivery::Latest::new());
            loop {
                let messages = updates.borrow_and_update().clone();
                #[cfg(test)]
                let observed = messages.clone();
                if delivery.submit(messages) {
                    let ui_for_update = ui.clone();
                    let current_generation = current_generation.clone();
                    let is_live = is_live.clone();
                    let account_id = account_id.clone();
                    let room_id = room_id.clone();
                    let root_event_id = root_event_id.clone();
                    let pending = delivery.clone();
                    let selected_account = selection.clone();
                    if ui_for_update
                        .upgrade_in_event_loop(move |ui| {
                            let Some(messages) = pending.take() else {
                                return;
                            };
                            if selected_account.is_current()
                                && is_live.load(Ordering::Acquire)
                                && current_generation.load(Ordering::Acquire) == generation
                                && ui.get_active_account_local_id().as_str() == account_id
                                && ui.get_active_room_id().as_str() == room_id
                                && ui.get_thread_root_id().as_str() == root_event_id
                            {
                                let replies = messages
                                    .iter()
                                    .filter(|message| {
                                        message.event_id.as_deref() != Some(root_event_id.as_str())
                                    })
                                    .cloned()
                                    .collect();
                                ui.set_thread_messages(crate::timeline_model::update_shared(
                                    ui.get_thread_messages(),
                                    replies,
                                ));
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                // Test builds only. Reached only after a successful enqueue:
                // this snapshot is held by a queued UI callback (queued just
                // above, or still pending from an earlier successful one).
                #[cfg(test)]
                timeline_controller_tests::callback_queued(
                    &account_id,
                    &room_id,
                    Some(root_event_id.as_str()),
                    &observed,
                );
                if !matches!(selection.guard(updates.changed()).await, Some(Ok(()))) {
                    break;
                }
            }
        });
        let mut current = self.thread_feed.lock().await;
        if self.thread_generation.load(Ordering::Acquire) == generation {
            *current = Some(handle);
        } else {
            handle.abort();
        }
        Ok(())
    }

    pub fn begin_thread_request(&self) -> u64 {
        self.thread_generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn thread_request_is_current(&self, generation: u64) -> bool {
        self.thread_generation.load(Ordering::Acquire) == generation
    }

    pub async fn clear_thread(&self) {
        let generation = self.begin_thread_request();
        self.clear_thread_if_current(generation).await;
    }

    pub async fn clear_thread_if_current(&self, generation: u64) {
        let mut feed = self.thread_feed.lock().await;
        if self.thread_generation.load(Ordering::Acquire) == generation {
            if let Some(task) = feed.take() {
                task.abort();
            }
        }
    }

    async fn require_selected_room(&self, room_id: &str) -> Result<(), MatrixCoreError> {
        if !self.is_live.load(Ordering::Acquire) {
            return Err(MatrixCoreError::NoActiveAccount);
        }
        if self.selected_room.lock().await.as_deref() != Some(room_id) {
            return Err(MatrixCoreError::SelectedRoomChanged);
        }
        Ok(())
    }

    pub async fn is_selected_room_for(&self, account_id: &str, room_id: &str) -> bool {
        self.is_live.load(Ordering::Acquire)
            && self.selected_room.lock().await.as_deref() == Some(room_id)
            && self
                .manager
                .active_account()
                .await
                .is_some_and(|account| account.id == account_id)
    }

    pub async fn require_media_context(
        &self,
        account_id: &str,
        room_id: &str,
    ) -> Result<(), MatrixCoreError> {
        self.require_selected_room(room_id).await?;
        if self
            .manager
            .active_account()
            .await
            .as_ref()
            .map(|account| account.id.as_str())
            != Some(account_id)
        {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        Ok(())
    }

    pub async fn send_attachment(
        &self,
        account_id: &str,
        room_id: &str,
        path: PathBuf,
        mime_type: &str,
    ) -> Result<QueuedAttachment, MatrixCoreError> {
        self.require_media_context(account_id, room_id).await?;
        let queued = self
            .manager
            .send_attachment_for(account_id, room_id, path, mime_type)
            .await?;
        if let Err(error) = self.require_media_context(account_id, room_id).await {
            let _ = queued.abort().await;
            return Err(error);
        }
        Ok(queued)
    }

    /// Queue one sticker while its account and room are still the visible
    /// ones. The core re-validates the catalog's selection, destination,
    /// consent and pack source and applies the room's send gate; like a text
    /// send, a queued sticker is never aborted merely because the view moved.
    pub async fn send_sticker(
        &self,
        catalog: &StickerCatalog,
        index: usize,
        consent: Option<&PublicMediaConsent>,
    ) -> Result<QueuedMessage, MatrixCoreError> {
        self.require_media_context(catalog.account_id(), catalog.room_id()).await?;
        self.manager.send_sticker_for(catalog, index, consent).await
    }

    pub async fn download_attachment(
        &self,
        account_id: &str,
        room_id: &str,
        event_id: &str,
    ) -> Result<DownloadedAttachment, MatrixCoreError> {
        self.require_media_context(account_id, room_id).await?;
        let downloaded = self
            .manager
            .download_attachment_for(account_id, room_id, event_id)
            .await?;
        self.require_media_context(account_id, room_id).await?;
        Ok(downloaded)
    }

    /// Commit decrypted bytes only while this exact account and room remain
    /// selected. Navigation and account switching wait for the bounded local
    /// write, preventing cross-account saves after an asynchronous download.
    pub async fn save_download_to(
        &self,
        account_id: &str,
        room_id: &str,
        path: PathBuf,
        bytes: Vec<u8>,
    ) -> Result<PathBuf, MatrixCoreError> {
        let _transition = self.account_transition.lock().await;
        let selected = self.selected_room.lock().await;
        if !self.is_live.load(Ordering::Acquire) || selected.as_deref() != Some(room_id) {
            return Err(MatrixCoreError::SelectedRoomChanged);
        }
        if self
            .manager
            .active_account()
            .await
            .as_ref()
            .map(|account| account.id.as_str())
            != Some(account_id)
        {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        tokio::task::spawn_blocking(move || save_download(&path, &bytes))
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?
            .map_err(MatrixCoreError::Io)
    }

    pub async fn send_composed(
        &self,
        selection: AccountSelection,
        room_id: String,
        target: ComposeSendTarget,
        text: ComposeText,
        baseline: Option<EditBaseline>,
        owner: Option<&Generation>,
    ) -> Result<QueuedMessage, MatrixCoreError> {
        self.require_selected_room(&room_id).await?;
        match &target {
            ComposeSendTarget::Room => {
                self.drafts
                    .persist_one(&self.manager, selection.account_id(), &room_id)
                    .await?
            }
            ComposeSendTarget::Thread(root) => {
                self.drafts
                    .persist_scoped(&self.manager, selection.account_id(), &room_id, Some(root))
                    .await?
            }
            _ => {}
        }
        self.manager
            .send_composed_owned_for(
                &selection,
                &room_id,
                target,
                &text,
                baseline.as_ref(),
                owner,
            )
            .await
    }

    pub async fn send(
        &self,
        account_id: String,
        room_id: String,
        body: String,
        target: MessageTarget,
        format: TextFormat,
    ) -> Result<Option<QueuedMessage>, MatrixCoreError> {
        self.require_selected_room(&room_id).await?;
        match target {
            MessageTarget::Room => {
                // Keep the original text durable until the send queue reports
                // a server acknowledgement or rejection.
                self.drafts
                    .persist_one(&self.manager, &account_id, &room_id)
                    .await?;
                self.manager
                    .send_formatted_tracked_for(&account_id, &room_id, body, format)
                    .await
                    .map(Some)
            }
            MessageTarget::Reply(event_id) => self
                .manager
                .send_reply_formatted_for(&account_id, &room_id, &event_id, body, format)
                .await
                .map(|_| None),
            MessageTarget::Edit(event_id) => self
                .manager
                .edit_formatted_for(&account_id, &room_id, &event_id, body, format)
                .await
                .map(|_| None),
            MessageTarget::Thread(root_event_id) => {
                self.drafts
                    .persist_scoped(&self.manager, &account_id, &room_id, Some(&root_event_id))
                    .await?;
                self.manager
                    .send_thread_formatted_tracked_for(
                        &account_id,
                        &room_id,
                        &root_event_id,
                        body,
                        format,
                    )
                    .await
                    .map(Some)
            }
        }
    }

    pub async fn toggle_reaction(
        &self,
        account_id: String,
        room_id: String,
        event_id: String,
        reaction: String,
    ) -> Result<bool, MatrixCoreError> {
        self.require_selected_room(&room_id).await?;
        self.manager
            .toggle_reaction_for(&account_id, &room_id, &event_id, &reaction)
            .await
    }

    pub async fn watch_updates(self: Arc<Self>, ui: Weak<AppWindow>) {
        let mut active = self.manager.subscribe_active_account();
        loop {
            let room_subscription = self.manager.subscribe_room_updates().await;
            match room_subscription {
                Ok((account_id, mut rooms)) => loop {
                    tokio::select! {
                        result = active.changed() => {
                            if result.is_err() { return; }
                            self.invalidate_navigation();
                            if self.is_live.load(Ordering::Acquire) {
                                self.clear_feed().await;
                                *self.selected_room.lock().await = None;
                                // Adoption under the transition lock, with the
                                // selection it belongs to.
                                let _transition = self.account_transition.lock().await;
                                if let Some(selection) = self
                                    .manager
                                    .active_account()
                                    .await
                                    .and_then(|account| self.manager.account_selection(&account.id))
                                {
                                    self.adopt_recovered_sends(selection, ui.clone()).await;
                                }
                                drop(_transition);
                                if let Err(error) = self.refresh(ui.clone()).await {
                                    show_error(&ui, error.to_string());
                                } else {
                                    tokio::spawn(self.clone().scan_spaces(ui.clone()));
                                }
                            }
                            break;
                        }
                        result = rooms.recv() => {
                            let Ok(update) = result else { break; };
                            if morpheus_matrix::room_updates_change_space_graph(&update) {
                                self.invalidate_space_scan().await;
                            }
                            if self.is_live.load(Ordering::Acquire)
                                && self.manager.active_account().await.is_some_and(|current| current.id == account_id)
                            {
                                if let Err(error) = self.refresh(ui.clone()).await {
                                    show_error(&ui, error.to_string());
                                } else {
                                    tokio::spawn(self.clone().scan_spaces(ui.clone()));
                                }
                            }
                        }
                    }
                },
                Err(_) => {
                    if active.changed().await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    pub async fn watch_sync(self: Arc<Self>, ui: Weak<AppWindow>) {
        let mut status = self.manager.subscribe_sync_status();
        while status.changed().await.is_ok() {
            if !self.is_live.load(Ordering::Acquire) {
                continue;
            }
            let label = match &*status.borrow_and_update() {
                SyncStatus::Stopped => "Sync stopped".to_owned(),
                SyncStatus::Starting => "Connecting to homeserver…".to_owned(),
                SyncStatus::Running {
                    backend: SyncBackend::SlidingSticky,
                } => String::new(),
                SyncStatus::Running {
                    backend: SyncBackend::Classic,
                } => "Chat connected; native calls require sliding sync on this homeserver"
                    .to_owned(),
                SyncStatus::Retrying { reason, .. } => format!("Reconnecting: {reason}"),
            };
            let _ = ui.upgrade_in_event_loop(move |ui| ui.set_session_error(label.into()));
        }
    }
}

struct Snapshot {
    accounts: Vec<AccountInfo>,
    active: AccountInfo,
    rooms: Vec<RoomInfo>,
    selected: Option<String>,
    selected_space: Option<String>,
    known_spaces: HashMap<String, String>,
    space_children: HashMap<String, HashSet<String>>,
    all_space_children: HashSet<String>,
    local_voice_rooms: HashSet<String>,
    shared_voice_rooms: HashSet<String>,
}

fn apply_snapshot(
    ui: &AppWindow,
    view: Snapshot,
    drafts: &DraftStore,
    security_gate: &crate::security::SecurityGate,
    voice_spaces: &VoiceSpaces,
) {
    let next_room_id = view.selected.as_deref().unwrap_or("");
    let account_changed = ui.get_active_account_local_id().as_str() != view.active.id;
    let close_sign_in = snapshot_closes_sign_in(ui.get_live_session(), account_changed);
    let selection_changed = account_changed || ui.get_active_room_id().as_str() != next_room_id;
    if account_changed {
        clear_account_context(ui, security_gate);
    }
    if selection_changed {
        ui.set_room_admin_open(false);
        ui.set_room_admin_operation(ui.get_room_admin_operation().wrapping_add(1));
        ui.set_room_admin_busy(false);
        ui.set_room_admin_error("".into());
        ui.set_room_admin_notice("".into());
        ui.set_room_admin_event_id("".into());
        ui.set_voice_designation_operation(ui.get_voice_designation_operation().wrapping_add(1));
        ui.set_voice_designation_busy(false);
        ui.set_voice_designation_error("".into());
        ui.set_voice_designation_notice("".into());
        ui.set_history_operation(ui.get_history_operation().wrapping_add(1));
        ui.set_history_loading(false);
        ui.set_history_at_start(false);
        ui.set_history_error("".into());
        ui.set_members(ModelRc::new(VecModel::<MemberItem>::from(Vec::new())));
        ui.set_member_loading(!next_room_id.is_empty());
        ui.set_member_error("".into());
        ui.set_member_action_operation(ui.get_member_action_operation().wrapping_add(1));
        ui.set_member_action_busy(false);
        ui.set_member_action_error("".into());
        ui.set_member_action_notice("".into());
        ui.set_member_invite_user_id("".into());
        ui.set_member_selected_user_id("".into());
        ui.set_member_selected_name("".into());
        ui.set_member_selected_membership("".into());
        ui.set_member_selected_power("".into());
        ui.set_member_selected_self(false);
        ui.set_member_selected_creator(false);
        ui.set_member_confirm_open(false);
        crate::reset_attachment_view(ui);
        crate::reset_room_search(ui);
        ui.set_room_call_mode(0);
        ui.set_composer_text(drafts.get(&view.active.id, next_room_id).into());
        ui.set_composer_markdown(drafts.markdown(&view.active.id, next_room_id));
        if drafts.take_rich_recovery_notice(&view.active.id, next_room_id, None) {
            ui.set_transfer_notice("Draft text restored; unsupported saved formatting and mention selections were discarded.".into());
        }
        ui.set_composer_saved_markdown(false);
        ui.set_composer_mode(0);
        ui.set_composer_target_id("".into());
        ui.set_composer_target_label("".into());
        ui.set_composer_saved_draft("".into());
        ui.set_composer_busy(false);
        draft_recovery::publish_room(
            ui,
            &drafts.pending_summary(&view.active.id, next_room_id, None),
            &drafts.pending_items(&view.active.id, next_room_id, None),
        );
        ui.set_composer_operation(ui.get_composer_operation().wrapping_add(1));
        ui.set_thread_root_id("".into());
        ui.set_thread_root_is_own(false);
        ui.set_thread_root_palette_index(0);
        ui.set_thread_root_accent(Color::default());
        ui.set_thread_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new())));
        ui.set_thread_composer_text("".into());
        ui.set_thread_markdown(false);
        draft_recovery::publish_thread(ui, &PendingSummary::default(), &[]);
    }
    apply_account_menu(ui, &view.accounts, &view.active);
    let active_space_name = view
        .selected_space
        .as_deref()
        .and_then(|id| view.rooms.iter().find(|room| room.room_id == id))
        .map(|room| room.name.as_str())
        .or_else(|| {
            view.selected_space
                .as_deref()
                .and_then(|id| view.known_spaces.get(id).map(String::as_str))
        })
        .unwrap_or("Home");
    ui.set_active_space_id(view.selected_space.as_deref().unwrap_or("").into());
    ui.set_active_space_name(active_space_name.into());
    let mut spaces = view
        .rooms
        .iter()
        .filter(|room| {
            (room.is_space || view.known_spaces.contains_key(&room.room_id))
                && room.membership == RoomMembership::Joined
        })
        .map(|room| SpaceItem {
            id: room.room_id.as_str().into(),
            name: view
                .known_spaces
                .get(&room.room_id)
                .map_or(room.name.as_str(), String::as_str)
                .into(),
            initials: initials(
                view.known_spaces
                    .get(&room.room_id)
                    .map_or(room.name.as_str(), String::as_str),
            )
            .into(),
            accent: accent(&room.room_id),
        })
        .collect::<Vec<_>>();
    for (id, name) in &view.known_spaces {
        if !spaces.iter().any(|space| space.id.as_str() == id) {
            spaces.push(SpaceItem {
                id: id.as_str().into(),
                name: name.as_str().into(),
                initials: initials(name).into(),
                accent: accent(id),
            });
        }
    }
    ui.set_spaces(ModelRc::new(VecModel::from(spaces)));
    let visible = |room: &RoomInfo| {
        if room.is_space
            || view.known_spaces.contains_key(&room.room_id)
            || room.membership != RoomMembership::Joined
        {
            return false;
        }
        if let Some(space_id) = view.selected_space.as_deref() {
            view.space_children
                .get(space_id)
                .is_some_and(|children| children.contains(&room.room_id))
        } else {
            room.is_direct || !view.all_space_children.contains(&room.room_id)
        }
    };
    ui.set_invitations(ModelRc::new(VecModel::from(
        view.rooms
            .iter()
            .filter(|room| room.membership == RoomMembership::Invited)
            .map(|room| room_item(room, false, false))
            .collect::<Vec<_>>(),
    )));
    ui.set_unsorted_channels(ModelRc::new(VecModel::from(
        view.rooms
            .iter()
            .filter(|room| !room.is_direct && visible(room))
            .map(|room| {
                room_item(
                    room,
                    view.local_voice_rooms.contains(&room.room_id),
                    view.shared_voice_rooms.contains(&room.room_id),
                )
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_unsorted_direct_messages(ModelRc::new(VecModel::from(
        view.rooms
            .iter()
            .filter(|room| room.is_direct && visible(room))
            .map(|room| {
                room_item(
                    room,
                    view.local_voice_rooms.contains(&room.room_id),
                    view.shared_voice_rooms.contains(&room.room_id),
                )
            })
            .collect::<Vec<_>>(),
    )));
    crate::apply_room_sort(ui);
    ui.set_stress_timeline_active(false);
    if let Some(room_id) = view.selected {
        if let Some(room) = view.rooms.iter().find(|room| room.room_id == room_id) {
            if selection_changed {
                ui.set_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new())));
            }
            ui.set_active_room_id(room.room_id.as_str().into());
            ui.set_active_room_name(room.name.as_str().into());
            ui.set_active_room_topic_raw(room.topic.as_deref().unwrap_or("").into());
            // Settings can change while this room snapshot is queued. Read
            // the same latest checked preference that a new join will capture.
            ui.set_room_call_mode(
                voice_spaces
                    .room_call_mode(&view.active.id, &room.room_id)
                    .index(),
            );
            ui.set_active_voice_local(view.local_voice_rooms.contains(&room.room_id));
            ui.set_active_voice_shared(view.shared_voice_rooms.contains(&room.room_id));
            ui.set_active_room_topic(if room.membership == RoomMembership::Invited {
                "Invitation · join to read messages".into()
            } else {
                room.topic.as_deref().unwrap_or("No room topic").into()
            });
        }
    } else {
        ui.set_active_room_id("".into());
        ui.set_active_room_name("No rooms yet".into());
        ui.set_active_room_topic_raw("".into());
        ui.set_room_call_mode(0);
        ui.set_active_voice_local(false);
        ui.set_active_voice_shared(false);
        ui.set_active_room_topic("Join or create a Matrix room to begin".into());
        ui.set_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new())));
    }
    ui.set_live_session(true);
    ui.set_preview_session(false);
    if account_changed && ui.get_page() == 1 {
        ui.invoke_refresh_security();
    }
    if close_sign_in {
        ui.set_login_loading(false);
        ui.set_login_error("".into());
        ui.set_show_sign_in(false);
    }
}

fn clear_account_context(ui: &AppWindow, security_gate: &crate::security::SecurityGate) {
    crate::security::clear_ui(ui, security_gate);
    ui.set_room_actions_open(false);
    ui.set_room_action_busy(false);
    ui.set_room_action_error("".into());
    ui.set_invite_error("".into());
    ui.set_room_action_operation(ui.get_room_action_operation().wrapping_add(1));
    ui.set_directory_operation(ui.get_directory_operation().wrapping_add(1));
    ui.set_directory_join_operation(ui.get_directory_join_operation().wrapping_add(1));
    ui.set_directory_loading(false);
    ui.set_directory_join_busy(false);
    ui.set_directory_joining_room_id("".into());
    ui.set_directory_searched(false);
    ui.set_directory_results(ModelRc::new(VecModel::from(Vec::<PublicRoomItem>::new())));
    ui.set_directory_next_batch("".into());
    ui.set_directory_error("".into());
    ui.set_directory_join_error("".into());
    ui.set_directory_search("".into());
    ui.set_directory_server("".into());
    ui.set_directory_join_input("".into());
    ui.set_invitations(ModelRc::new(VecModel::<RoomItem>::default()));
    ui.set_quick_switcher_results(ModelRc::new(VecModel::<RoomItem>::default()));
    ui.set_quick_switcher_open(false);
    ui.set_quick_switcher_index(-1);
    ui.set_quick_switcher_match_count(0);
    ui.set_quick_switcher_account_index(-1);
    ui.set_member_loading(false);
    ui.set_member_action_busy(false);
    ui.set_member_confirm_open(false);
    ui.set_member_action_operation(ui.get_member_action_operation().wrapping_add(1));
    ui.set_quick_switcher_query("".into());
    ui.set_quick_switcher_account_id("".into());
    ui.set_quick_switcher_space_id("".into());
    ui.set_member_invite_user_id("".into());
    ui.set_member_selected_user_id("".into());
    ui.set_member_selected_name("".into());
    ui.set_member_selected_membership("".into());
    ui.set_member_selected_power("".into());
    ui.set_member_confirm_user_id("".into());
    ui.set_member_confirm_name("".into());
    ui.set_member_confirm_value("".into());
    ui.set_member_confirm_account_id("".into());
    ui.set_member_confirm_room_id("".into());
    ui.set_member_error("".into());
    ui.set_member_action_error("".into());
    ui.set_member_action_notice("".into());
    ui.set_active_room_topic_raw("".into());
    ui.set_room_admin_open(false);
    ui.set_room_admin_busy(false);
    ui.set_room_admin_operation(ui.get_room_admin_operation().wrapping_add(1));
    ui.set_room_admin_event_id("".into());
    ui.set_room_admin_event_preview("".into());
    ui.set_room_admin_account_id("".into());
    ui.set_room_admin_room_id("".into());
    ui.set_room_admin_name_input("".into());
    ui.set_room_admin_topic_input("".into());
    ui.set_room_admin_error("".into());
    ui.set_room_admin_notice("".into());
    ui.set_voice_designation_operation(ui.get_voice_designation_operation().wrapping_add(1));
    ui.set_voice_designation_busy(false);
    ui.set_voice_designation_error("".into());
    ui.set_voice_designation_notice("".into());
    ui.set_history_operation(ui.get_history_operation().wrapping_add(1));
    ui.set_history_loading(false);
    ui.set_history_at_start(false);
    ui.set_history_error("".into());
    ui.set_transfer_notice("".into());
}

pub(crate) fn apply_account_menu(ui: &AppWindow, accounts: &[AccountInfo], active: &AccountInfo) {
    let Some(active_index) = accounts.iter().position(|account| account.id == active.id) else {
        return;
    };
    let items = accounts
        .iter()
        .enumerate()
        .map(|(index, account)| account_item(account, index == active_index))
        .collect::<Vec<_>>();
    let options = std::iter::once(active_index)
        .chain((0..accounts.len()).filter(|index| *index != active_index))
        .map(|index| {
            let item = &items[index];
            AccountOption {
                local_id: accounts[index].id.as_str().into(),
                index: index as i32,
                name: item.name.clone(),
                handle: item.handle.clone(),
                initials: item.initials.clone(),
                accent: item.accent,
            }
        })
        .collect::<Vec<_>>();
    let account = &items[active_index];
    ui.set_accounts(ModelRc::new(VecModel::from(items.clone())));
    ui.set_account_options(ModelRc::new(VecModel::from(options)));
    ui.set_active_account_index(active_index as i32);
    ui.set_active_account_local_id(active.id.as_str().into());
    // The theme follows the account this accepted snapshot publishes. A
    // superseded activation refresh returns without reaching here, so only
    // the winning snapshot's account can select the palette.
    crate::appearance::apply_theme_mode(ui);
    ui.set_active_account_name(account.name.clone());
    ui.set_active_account_handle(account.handle.clone());
    ui.set_active_account_initials(account.initials.clone());
    ui.set_active_account_status(account.status.clone());
    ui.set_active_account_accent(account.accent);
}

fn account_item(account: &AccountInfo, selected: bool) -> AccountItem {
    let name = display_user(&account.user_id);
    let account_initials = initials(&name);
    AccountItem {
        name: name.into(),
        handle: account.user_id.as_str().into(),
        initials: account_initials.into(),
        status: if selected { "Connected" } else { "Paused" }.into(),
        accent: accent(&account.id),
    }
}

fn room_item(room: &RoomInfo, voice_local: bool, voice_shared: bool) -> RoomItem {
    RoomItem {
        id: room.room_id.as_str().into(),
        name: room.name.as_str().into(),
        unread: room.unread_notifications.min(i32::MAX as u64) as i32,
        mentions: room.unread_mentions.min(i32::MAX as u64) as i32,
        private_room: room.is_direct,
        voice_space: voice_local || voice_shared,
        voice_local,
        voice_shared,
    }
}

fn apply_shared_voice_labels(ui: &AppWindow, shared_rooms: &HashSet<String>) {
    let update = |mut room: RoomItem| {
        room.voice_shared = shared_rooms.contains(room.id.as_str());
        room.voice_space = room.voice_local || room.voice_shared;
        room
    };
    ui.set_unsorted_channels(ModelRc::new(VecModel::from(
        ui.get_unsorted_channels()
            .iter()
            .map(update)
            .collect::<Vec<_>>(),
    )));
    ui.set_unsorted_direct_messages(ModelRc::new(VecModel::from(
        ui.get_unsorted_direct_messages()
            .iter()
            .map(update)
            .collect::<Vec<_>>(),
    )));
    ui.set_active_voice_shared(shared_rooms.contains(ui.get_active_room_id().as_str()));
    crate::apply_room_sort(ui);
}

fn member_item(info: &RoomMemberInfo) -> MemberItem {
    let user_id = crate::directory_plain_text(&info.user_id, 260);
    let name = info
        .display_name
        .as_deref()
        .map(|value| crate::directory_plain_text(value, 100))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| crate::directory_plain_text(&display_user(&info.user_id), 100));
    let membership = match info.membership.as_str() {
        "join" => match info.power_level {
            None => "Creator".to_owned(),
            Some(level) if level >= 100 => format!("Admin ({level})"),
            Some(level) if level >= 50 => format!("Moderator ({level})"),
            Some(level) if level > 0 => format!("Power {level}"),
            _ => "Member".to_owned(),
        },
        "invite" => "Invited".to_owned(),
        "ban" => "Banned".to_owned(),
        _ => "Member".to_owned(),
    };
    let detail = format!(
        "{} · {}{}",
        user_id,
        if info.is_account_user { "You · " } else { "" },
        membership
    );
    MemberItem {
        user_id: user_id.into(),
        name: name.clone().into(),
        initials: initials(&name).into(),
        detail: detail.into(),
        membership: info.membership.as_str().into(),
        power_level_text: info
            .power_level
            .map_or_else(String::new, |level| level.to_string())
            .into(),
        is_account_user: info.is_account_user,
        is_creator: info.membership == "join" && info.power_level.is_none(),
        accent: accent(&info.user_id),
        online: false,
    }
}

pub(super) fn message_item(info: &MessageInfo) -> MessageItem {
    let sender = display_user(&info.sender);
    let sender_initials = initials(&sender);
    let clocks =
        crate::message_metadata::ClockLabels::for_message(info.timestamp_ms, info.send_state);
    let display_body = if info.attachment_name.as_deref() == Some(info.body.as_str()) {
        ""
    } else {
        info.body.as_str()
    };
    let styled_markdown = if display_body.is_empty() {
        None
    } else {
        info.rich_markdown
            .as_deref()
            .and_then(|markdown| StyledText::from_markdown(markdown).ok())
    };
    let rich_accessible_body = if styled_markdown.is_some() {
        info.rich_accessible
            .as_deref()
            .filter(|text| !text.is_empty())
            .unwrap_or(display_body)
    } else {
        display_body
    };
    // Use the accepted formatted projection as the edit source. The plain
    // Matrix body may be an unrelated fallback, so editing it would silently
    // discard visible styling when an unchanged message is saved.
    let edit_markdown = styled_markdown.is_some();
    let edit_source = if edit_markdown {
        info.rich_markdown.as_deref().unwrap_or(display_body)
    } else {
        display_body
    };
    let rich_body = styled_markdown.unwrap_or_else(|| StyledText::from_plain_text(display_body));
    MessageItem {
        event_id: info.event_id.as_deref().unwrap_or("").into(),
        is_own: info.is_own,
        sender: sender.into(),
        sender_palette_index: crate::appearance::sender_palette_index(&info.sender),
        initials: sender_initials.into(),
        time: clocks.twelve_hour.into(),
        time_24h: clocks.twenty_four_hour.into(),
        time_24h_seconds: clocks.twenty_four_hour_seconds.into(),
        body: display_body.into(),
        rich_body,
        rich_accessible_body: rich_accessible_body.into(),
        edit_source: edit_source.into(),
        edit_markdown,
        reply_to: info.reply_preview.as_deref().unwrap_or("").into(),
        attachment: info.attachment_name.as_deref().unwrap_or("").into(),
        attachment_is_image: info.attachment_is_image,
        reaction: info.reaction_summary.as_str().into(),
        accent: accent(&info.sender),
    }
}

fn display_user(id: &str) -> String {
    id.trim_start_matches('@')
        .split(':')
        .next()
        .unwrap_or(id)
        .to_owned()
}

fn initials(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_alphanumeric())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect()
}

fn accent(value: &str) -> Color {
    const COLORS: [(u8, u8, u8); 5] = [
        (168, 140, 255),
        (120, 200, 177),
        (243, 159, 120),
        (100, 165, 232),
        (231, 139, 164),
    ];
    let hash = value.bytes().fold(0usize, |current, byte| {
        current.wrapping_mul(31).wrapping_add(byte as usize)
    });
    let (red, green, blue) = COLORS[hash % COLORS.len()];
    Color::from_rgb_u8(red, green, blue)
}

pub fn show_error(ui: &Weak<AppWindow>, error: String) {
    let _ = ui.upgrade_in_event_loop(move |ui| ui.set_session_error(error.into()));
}

fn visible_feed_target(
    visible_account: &str,
    visible_room: &str,
    expected_account: &str,
    expected_room: &str,
) -> bool {
    visible_account == expected_account && visible_room == expected_room
}

fn snapshot_closes_sign_in(was_live: bool, account_changed: bool) -> bool {
    !was_live || account_changed
}

#[cfg(test)]
mod member_tests {
    use super::*;

    #[test]
    fn member_rows_sanitize_untrusted_identity_and_do_not_claim_presence() {
        let item = member_item(&RoomMemberInfo {
            user_id: "@alice:example.org".into(),
            display_name: Some("Alice\u{202e}Admin\u{200b}".into()),
            avatar_mxc: None,
            membership: "join".into(),
            power_level: Some(50),
            is_account_user: true,
        });
        assert_eq!(item.name.as_str(), "Alice Admin");
        assert_eq!(
            item.detail.as_str(),
            "@alice:example.org · You · Moderator (50)"
        );
        assert!(!item.online);

        let invited = member_item(&RoomMemberInfo {
            user_id: "@bob:example.org".into(),
            display_name: None,
            avatar_mxc: None,
            membership: "invite".into(),
            power_level: Some(0),
            is_account_user: false,
        });
        assert_eq!(invited.name.as_str(), "bob");
        assert_eq!(invited.detail.as_str(), "@bob:example.org · Invited");
    }

    #[test]
    fn feed_updates_require_the_visible_account_and_room() {
        assert!(visible_feed_target(
            "account-a",
            "!room-a",
            "account-a",
            "!room-a"
        ));
        assert!(!visible_feed_target(
            "account-b",
            "!room-a",
            "account-a",
            "!room-a"
        ));
        assert!(!visible_feed_target(
            "account-a",
            "!room-b",
            "account-a",
            "!room-a"
        ));
    }

    #[test]
    fn background_refresh_keeps_add_account_form_open() {
        assert!(!snapshot_closes_sign_in(true, false));
        assert!(snapshot_closes_sign_in(true, true));
        assert!(snapshot_closes_sign_in(false, false));
    }

    #[test]
    fn formatted_edit_uses_visible_source_and_matching_accessibility_text() {
        let info = MessageInfo {
            key: "message".into(),
            event_id: Some("$event".into()),
            sender: "@alice:example.org".into(),
            body: "different plain fallback".into(),
            rich_markdown: Some("Visible **bold** text".into()),
            rich_accessible: Some("Visible bold text".into()),
            attachment_name: None,
            attachment_is_image: false,
            reply_preview: None,
            reaction_summary: String::new(),
            timestamp_ms: 0,
            is_own: true,
            is_local_echo: false,
            send_state: MessageSendState::Sent,
        };
        let item = message_item(&info);
        assert!(item.edit_markdown);
        let other_body = message_item(&MessageInfo {
            body: "Another message".into(),
            rich_markdown: None,
            rich_accessible: None,
            ..info.clone()
        });
        assert_eq!(item.sender_palette_index, other_body.sender_palette_index);
        let other_server = message_item(&MessageInfo {
            sender: "@alice:other.example.org".into(),
            ..info.clone()
        });
        assert_eq!(item.sender, other_server.sender);
        assert_ne!(item.sender_palette_index, other_server.sender_palette_index);
        assert_eq!(item.edit_source.as_str(), "Visible **bold** text");
        assert_eq!(item.rich_accessible_body.as_str(), "Visible bold text");

        let plain = message_item(&MessageInfo {
            rich_markdown: None,
            rich_accessible: None,
            ..info
        });
        assert!(!plain.edit_markdown);
        assert_eq!(plain.edit_source.as_str(), "different plain fallback");
        assert_eq!(
            plain.rich_accessible_body.as_str(),
            "different plain fallback"
        );
    }
}

#[cfg(test)]
mod account_presentation_tests {
    use super::*;

    fn empty_profile() -> (tempfile::TempDir, Arc<LiveUi>) {
        let directory = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(directory.path().join("profile")).unwrap());
        let voices = Arc::new(VoiceSpaces::new(morpheus_config::ConfigStore::at(
            directory.path().join("config.toml"),
        )));
        let live = LiveUi::new(manager, Handle::current(), Weak::default(), voices);
        (directory, live)
    }

    struct SignalDrop(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for SignalDrop {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    async fn pending_feed() -> (JoinHandle<()>, tokio::sync::oneshot::Receiver<()>) {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _on_drop = SignalDrop(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        // Start the real task before revocation so this tests cancellation of
        // retained work, rather than only dropping an unpolled future.
        started_rx.await.unwrap();
        (task, dropped_rx)
    }

    async fn assert_feed_cancelled(receiver: tokio::sync::oneshot::Receiver<()>) {
        tokio::time::timeout(Duration::from_secs(2), receiver)
            .await
            .expect("revoked feed must be cancelled")
            .expect("feed cancellation must drop its retained work");
    }

    async fn close_profile(directory: tempfile::TempDir, live: Arc<LiveUi>) {
        let manager = live.manager.clone();
        drop(live);
        // LiveUi's existing draft worker owns the profile until its channel
        // closes. Drain that owner before removing the locked file on Windows.
        tokio::time::timeout(Duration::from_secs(2), async {
            while Arc::strong_count(&manager) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("empty draft worker must release the profile");
        drop(manager);
        directory.close().unwrap();
    }

    #[tokio::test]
    async fn detach_revokes_room_thread_and_navigation_work() {
        let (directory, live) = empty_profile();
        live.is_live.store(true, Ordering::Release);
        *live.selected_room.lock().await = Some("!old-room:example.org".into());
        *live.selected_space.lock().await = Some("!old-space:example.org".into());
        *live.feed_key.lock().await = Some(("old-account".into(), "!old-room:example.org".into()));
        live.space_children.lock().await.insert(
            "!old-space:example.org".into(),
            HashSet::from(["!old-room:example.org".into()]),
        );
        *live.space_scan_key.lock().await =
            Some(("old-account".into(), vec!["!old-space:example.org".into()]));
        *live.space_scan_failed_at.lock().await = Some(Instant::now());

        let navigation = live.current_navigation_epoch();
        let render = live.render_epoch.load(Ordering::Acquire);
        let member = live.member_epoch.load(Ordering::Acquire);
        let scan = live.space_scan_epoch.load(Ordering::Acquire);
        let room_feed = live.generation.load(Ordering::Acquire);
        let thread = live.begin_thread_request();
        assert!(live.navigation_is_current(navigation));
        assert!(live.thread_request_is_current(thread));
        let (feed, feed_cancelled) = pending_feed().await;
        let (thread_feed, thread_cancelled) = pending_feed().await;
        *live.feed.lock().await = Some(feed);
        *live.thread_feed.lock().await = Some(thread_feed);

        live.account_detached(Weak::default()).await;

        assert!(!live.is_live.load(Ordering::Acquire));
        assert!(!live.navigation_is_current(navigation));
        assert!(!live.thread_request_is_current(thread));
        assert!(live.render_epoch.load(Ordering::Acquire) > render);
        assert!(live.member_epoch.load(Ordering::Acquire) > member);
        assert!(live.space_scan_epoch.load(Ordering::Acquire) > scan);
        assert!(live.generation.load(Ordering::Acquire) > room_feed);
        assert!(live.selected_room.lock().await.is_none());
        assert!(live.selected_space.lock().await.is_none());
        assert!(live.feed.lock().await.is_none());
        assert!(live.thread_feed.lock().await.is_none());
        assert!(live.feed_key.lock().await.is_none());
        assert!(live.space_scan_key.lock().await.is_none());
        assert!(live.space_scan_failed_at.lock().await.is_none());
        assert!(live.space_children.lock().await.is_empty());
        assert!(matches!(
            live.require_media_context("old-account", "!old-room:example.org")
                .await,
            Err(MatrixCoreError::NoActiveAccount)
        ));
        assert_feed_cancelled(feed_cancelled).await;
        assert_feed_cancelled(thread_cancelled).await;
        close_profile(directory, live).await;
    }

    #[tokio::test]
    async fn failed_activation_presentation_returns_to_detached_state() {
        let (directory, live) = empty_profile();
        // Simulate stale presentation state without granting core account
        // authority. The real activation adapter must fail closed.
        live.is_live.store(true, Ordering::Release);
        *live.selected_room.lock().await = Some("!old-room:example.org".into());
        *live.selected_space.lock().await = Some("!old-space:example.org".into());
        let navigation = live.current_navigation_epoch();
        let (feed, feed_cancelled) = pending_feed().await;
        let (thread_feed, thread_cancelled) = pending_feed().await;
        *live.feed.lock().await = Some(feed);
        *live.thread_feed.lock().await = Some(thread_feed);

        assert!(matches!(
            live.account_activated(Weak::default()).await,
            Err(MatrixCoreError::NoActiveAccount)
        ));

        assert!(live.manager.active_account().await.is_none());
        assert!(!live.is_live.load(Ordering::Acquire));
        assert!(!live.navigation_is_current(navigation));
        assert!(live.selected_room.lock().await.is_none());
        assert!(live.selected_space.lock().await.is_none());
        assert!(live.feed.lock().await.is_none());
        assert!(live.thread_feed.lock().await.is_none());
        assert_feed_cancelled(feed_cancelled).await;
        assert_feed_cancelled(thread_cancelled).await;
        close_profile(directory, live).await;
    }
}

/// Opt-in regression against the local Synapse fixture (the same
/// `MORPHEUS_MATRIX_TEST_ACCOUNTS` file the core fixture reads). It drives the
/// real app observer and completion helpers with real selections from the
/// account manager, revokes them by an actual account switch and A->B->A, and
/// proves the departed selection's observers end and change nothing without
/// any further SDK event.
#[cfg(test)]
mod stale_observer_tests {
    use super::*;
    use crate::draft_recovery::SendScope;
    use matrix_sdk::{
        ruma::{RoomId, events::room::message::RoomMessageEventContent, serde::Raw},
        send_queue::{LocalEchoContent, RoomSendQueueUpdate},
    };

    // Fixed tokens only: no account IDs, message text or server errors.
    macro_rules! recovery_phase {
        ($case:ident) => {
            eprintln!("\nMORPHEUS_APP_RECOVERY_STAGE {}", stringify!($case));
        };
    }
    macro_rules! recovery_check {
        ($case:ident) => {
            eprintln!("\nMORPHEUS_APP_RECOVERY_CHECK {}=true", stringify!($case));
        };
    }

    /// A fixture-owned hold on one room's SDK queue (see the core fixture's
    /// `QueueHold`): a request the homeserver refuses permanently, recorded by
    /// the SDK as wedged at the head, so every later request stays persisted
    /// whatever the product does to the room's enabled flag. Established only
    /// once the SDK reports the refusal; a delivered hold fails the test.
    struct QueueHold {
        transaction_id: String,
        handle: matrix_sdk::send_queue::SendHandle,
    }

    impl QueueHold {
        async fn place(room: &matrix_sdk::Room) -> Self {
            let (echoes_before, mut updates) = room
                .send_queue()
                .subscribe()
                .await
                .expect("Cannot observe the room send queue");
            assert!(
                echoes_before.is_empty(),
                "a hold is placed only on an empty room queue"
            );
            let content = Raw::new(&RoomMessageEventContent::text_plain("x".repeat(128 * 1024)))
                .expect("Cannot encode the hold event")
                .cast();
            let handle = room
                .send_queue()
                .send_raw(content, "m.room.message".into())
                .await
                .expect("Cannot enqueue the hold event");
            let transaction_id = handle.transaction_id().to_string();
            room.send_queue().set_enabled(true);
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    match updates.recv().await {
                        Ok(RoomSendQueueUpdate::SendError {
                            transaction_id: failed,
                            is_recoverable,
                            ..
                        }) if failed.as_str() == transaction_id => {
                            assert!(!is_recoverable, "the hold must be a permanent refusal");
                            return;
                        }
                        Ok(RoomSendQueueUpdate::SentEvent {
                            transaction_id: sent,
                            ..
                        }) if sent.as_str() == transaction_id => {
                            panic!("The hold event was delivered; nothing holds the queue");
                        }
                        Ok(_) => {}
                        Err(error) => panic!("Send queue observation ended: {error}"),
                    }
                }
            })
            .await
            .expect("Timed out waiting for the SDK to refuse the hold event");
            room.send_queue().set_enabled(false);
            let (echoes, _) = room
                .send_queue()
                .subscribe()
                .await
                .expect("Cannot observe the room send queue");
            let head = echoes.first().expect("the hold left the SDK queue");
            assert_eq!(
                head.transaction_id.as_str(),
                transaction_id,
                "the hold must be the head"
            );
            assert!(
                matches!(
                    &head.content,
                    LocalEchoContent::Event {
                        send_error: Some(_),
                        ..
                    }
                ),
                "the hold is not recorded as wedged"
            );
            Self {
                transaction_id,
                handle,
            }
        }

        async fn release(self, room: &matrix_sdk::Room) {
            assert!(
                self.handle
                    .abort()
                    .await
                    .expect("Cannot abort the hold event"),
                "the hold was already gone"
            );
            let (echoes, _) = room
                .send_queue()
                .subscribe()
                .await
                .expect("Cannot observe the room send queue");
            assert!(
                !echoes
                    .iter()
                    .any(|echo| echo.transaction_id.as_str() == self.transaction_id),
                "the hold must leave the SDK queue on release"
            );
        }
    }

    /// Release the profile the way the app does: the draft worker holds the
    /// manager until its channel closes.
    async fn close_profile(directory: tempfile::TempDir, live: Arc<LiveUi>) {
        let manager = live.manager.clone();
        drop(live);
        tokio::time::timeout(Duration::from_secs(2), async {
            while Arc::strong_count(&manager) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the draft worker must release the profile");
        drop(manager);
        directory.close().unwrap();
    }

    struct FixtureAccount {
        homeserver_url: String,
        username: String,
        password: String,
    }

    fn fixture() -> Option<(FixtureAccount, FixtureAccount, String)> {
        let path = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS")?;
        let fixture: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("Cannot read local fixture"))
                .expect("Invalid local fixture");
        let account = |label: &str| {
            let entry = fixture["accounts"]
                .as_array()
                .expect("fixture accounts")
                .iter()
                .find(|entry| entry["label"].as_str() == Some(label))
                .expect("fixture account");
            FixtureAccount {
                homeserver_url: entry["homeserver_url"].as_str().unwrap().to_owned(),
                username: entry["username"].as_str().unwrap().to_owned(),
                password: entry["password"].as_str().unwrap().to_owned(),
            }
        };
        let room = fixture["rooms"]["a_room"]
            .as_str()
            .expect("fixture room")
            .to_owned();
        Some((account("a_primary"), account("b_primary"), room))
    }

    async fn wait_for_room(manager: &AccountManager, room_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if manager
                .rooms_snapshot()
                .await
                .expect("Cannot read rooms")
                .iter()
                .any(|room| room.room_id == room_id)
            {
                opened_room::scan(manager, room_id)
                    .await
                    .expect("Opened fixture room must be scanned");
                return;
            }
            assert!(Instant::now() < deadline, "Timed out waiting for room");
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    #[tokio::test]
    async fn stale_observers_end_on_revocation_and_never_touch_the_departed_cache() {
        let Some((alice, carol, a_room)) = fixture() else {
            eprintln!(
                "Skipped stale observer regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
            );
            return;
        };
        recovery_phase!(observer_revocation);
        let directory = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(directory.path().join("profile")).unwrap());
        let voices = Arc::new(VoiceSpaces::new(morpheus_config::ConfigStore::at(
            directory.path().join("config.toml"),
        )));
        let live = LiveUi::new(manager.clone(), Handle::current(), Weak::default(), voices);
        let a_account = manager
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("First login failed");
        wait_for_room(&manager, &a_room).await;
        let selection = manager
            .account_selection(&a_account.id)
            .expect("Selection missing");
        // Discovery must cover the room before a recorded send; this is the
        // same wait the controller performs at activation.
        manager.take_recovered_drafts(&selection).await.unwrap();
        live.is_live.store(true, Ordering::Release);
        *live.selected_room.lock().await = Some(a_room.clone());
        // Hold: a wedged head keeps the recorded request persisted and the SDK
        // never emits for it, so every observer below ends only through
        // revocation. The product's own per-room resume cannot send past it.
        let client = manager.active_client().await.unwrap();
        let room = client
            .get_room(&RoomId::parse(&a_room).unwrap())
            .expect("Room missing");
        let hold = QueueHold::place(&room).await;

        let scope = SendScope {
            account: a_account.id.clone(),
            room: a_room.clone(),
            thread: None,
        };
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let text = ComposeText::plain(format!("stale-observer-{unique}"));
        live.drafts.set(&a_account.id, &a_room, &text.body);
        let revision = live.drafts.revision(&a_account.id, &a_room).unwrap();
        let handle = live
            .prepare_pending_send(&selection, &scope, &text, revision)
            .await
            .expect("the record is durable before enqueue");
        let queued = live
            .send_composed(
                selection.clone(),
                a_room.clone(),
                ComposeSendTarget::Room,
                text.clone(),
                None,
                Some(handle.nonce()),
            )
            .await
            .expect("enqueue under the current selection");
        let transaction = queued.transaction_id().to_owned();
        assert_eq!(
            live.record_pending_queued(&selection, &scope, &handle, &transaction)
                .await,
            Some(true)
        );
        let nonce = handle.nonce().clone();
        let queued_status = || {
            live.drafts
                .record_status(&a_account.id, &a_room, None, &nonce)
        };
        assert_eq!(queued_status(), Some(PendingStatus::Queued));
        assert_eq!(
            live.drafts
                .record_transaction_id(&a_account.id, &a_room, None, &nonce),
            Some(Some(transaction.clone()))
        );

        // The real observer waits on the paused queue: it holds the live
        // controller and the SDK handle while it waits.
        let idle_holds = Arc::strong_count(&live);
        draft_recovery::spawn_observer(
            live.clone(),
            Weak::default(),
            selection.clone(),
            scope.clone(),
            nonce.clone(),
            transaction.clone(),
            queued,
        );
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            Arc::strong_count(&live) > idle_holds,
            "the observer holds its resources while waiting"
        );

        // A->B->A: adding the second account revokes the first selection;
        // switching back issues a new one. No SDK event is produced for the
        // persisted request at any point.
        let b_account = manager
            .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
            .await
            .expect("Second login failed");
        assert!(!selection.is_current());
        manager
            .switch_to(&a_account.id)
            .await
            .expect("Cannot switch back");
        let renewed = manager
            .account_selection(&a_account.id)
            .expect("Renewed selection missing");
        assert!(renewed.is_current());
        tokio::time::timeout(Duration::from_secs(5), async {
            while Arc::strong_count(&live) != idle_holds {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the stale observer must end on revocation and release what it held");
        assert_eq!(
            queued_status(),
            Some(PendingStatus::Queued),
            "ending changed nothing"
        );

        // Late completions under the departed selection (the core call
        // returning after revocation, or an acknowledgement arriving late)
        // change nothing in the cache and schedule no write.
        assert!(
            live.apply_pending_update(
                &selection,
                &scope,
                &nonce,
                &transaction,
                QueuedSendUpdate::Sent,
                &Weak::default()
            )
            .await
        );
        live.mark_pending_unconfirmed(&selection, &scope, &nonce)
            .await;
        live.abandon_pending_send(&selection, &scope, &nonce).await;
        assert_eq!(
            live.record_pending_queued(&selection, &scope, &handle, &transaction)
                .await,
            None
        );
        assert_eq!(queued_status(), Some(PendingStatus::Queued));
        assert_eq!(
            live.drafts
                .record_transaction_id(&a_account.id, &a_room, None, &nonce),
            Some(Some(transaction.clone()))
        );
        // The saved state is intact for the renewed selection: discovery
        // adopts exactly the persisted request, so nothing valid was dropped.
        let mut recovered = manager.take_recovered_drafts(&renewed).await.unwrap();
        let send = recovered
            .iter_mut()
            .find(|draft| draft.room_id == a_room && draft.thread_root.is_none())
            .and_then(|draft| draft.sends.iter_mut().find(|send| send.nonce == nonce))
            .expect("the record survives the switch");
        assert_eq!(send.transaction_id.as_deref(), Some(transaction.as_str()));
        let adopted = send
            .queued
            .take()
            .expect("the exact persisted request is adopted");
        // The hold is the fixture's: lift it, let the adopted request complete
        // under the renewed selection, then release the SDK handles before the
        // accounts are removed.
        hold.release(&room).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), adopted.wait_outcome())
                .await
                .expect("Timed out waiting for the adopted send"),
            morpheus_matrix::QueuedSendOutcome::Sent
        );
        drop(room);
        drop(client);
        drop(recovered);

        manager
            .logout_account(&a_account.id)
            .await
            .expect("Cannot remove first account");
        manager
            .logout_account(&b_account.id)
            .await
            .expect("Cannot remove second account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
        recovery_check!(observer_revocation);
    }

    /// A send preparation interrupted by an account switch. The enqueuing
    /// record it committed stays in the row for discovery; the stale
    /// continuation's rollback changes nothing in the departed cache and
    /// schedules no write; the renewed selection re-seeds the record as
    /// unconfirmed; and the account-bound flush still succeeds afterwards, so
    /// the cache and the row never diverge.
    #[tokio::test]
    async fn interrupted_prepare_keeps_its_committed_record_for_discovery_and_leaves_the_cache_consistent()
     {
        let Some((alice, carol, a_room)) = fixture() else {
            eprintln!(
                "Skipped interrupted prepare regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
            );
            return;
        };
        recovery_phase!(interrupted_prepare);
        let directory = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(directory.path().join("profile")).unwrap());
        let voices = Arc::new(VoiceSpaces::new(morpheus_config::ConfigStore::at(
            directory.path().join("config.toml"),
        )));
        let live = LiveUi::new(manager.clone(), Handle::current(), Weak::default(), voices);
        let a_account = manager
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("First login failed");
        wait_for_room(&manager, &a_room).await;
        let selection = manager
            .account_selection(&a_account.id)
            .expect("Selection missing");
        manager.take_recovered_drafts(&selection).await.unwrap();
        live.is_live.store(true, Ordering::Release);
        *live.selected_room.lock().await = Some(a_room.clone());
        let scope = SendScope {
            account: a_account.id.clone(),
            room: a_room.clone(),
            thread: None,
        };
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let text = ComposeText::plain(format!("interrupted-prepare-{unique}"));
        live.drafts.set(&a_account.id, &a_room, &text.body);
        let revision = live.drafts.revision(&a_account.id, &a_room).unwrap();
        // The preparation completes under the first selection: the enqueuing
        // record is in the row and granted, but the send never happens.
        let handle = live
            .prepare_pending_send(&selection, &scope, &text, revision)
            .await
            .expect("the record is durable before enqueue");
        let nonce = handle.nonce().clone();
        let status = || {
            live.drafts
                .record_status(&a_account.id, &a_room, None, &nonce)
        };
        assert_eq!(status(), Some(PendingStatus::Enqueuing));
        let in_row = || async {
            manager
                .load_composed_draft_for(&a_account.id, &a_room, None)
                .await
                .unwrap()
                .envelope
                .find(&nonce)
                .map(|record| record.is_queued())
        };
        assert_eq!(in_row().await, Some(false), "committed as enqueuing");
        // A thread send prepared the same way: the room index is durable
        // before the thread record (the record is never left unindexed), and
        // the index entry this preparation added is recorded as its own.
        let root = "$interrupted-thread:fixture.invalid";
        live.drafts
            .set_scoped(&a_account.id, &a_room, Some(root), "thread reply");
        let thread_scope = SendScope {
            account: a_account.id.clone(),
            room: a_room.clone(),
            thread: Some(root.to_owned()),
        };
        let thread_revision = live
            .drafts
            .scoped_revision(&a_account.id, &a_room, Some(root))
            .unwrap();
        let thread_handle = live
            .prepare_pending_send(
                &selection,
                &thread_scope,
                &ComposeText::plain("thread reply"),
                thread_revision,
            )
            .await
            .expect("the thread record is durable before enqueue");
        let thread_nonce = thread_handle.nonce().clone();
        let room_row = manager
            .load_composed_draft_for(&a_account.id, &a_room, None)
            .await
            .unwrap();
        assert!(
            room_row
                .envelope
                .threads()
                .iter()
                .any(|known| known == root),
            "the room index anchors the thread"
        );
        assert_eq!(
            manager
                .load_composed_draft_for(&a_account.id, &a_room, Some(root))
                .await
                .unwrap()
                .envelope
                .find(&thread_nonce)
                .map(|record| record.is_queued()),
            Some(false)
        );

        // A -> B -> A while the preparation's continuation is still pending.
        let b_account = manager
            .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
            .await
            .expect("Second login failed");
        assert!(!selection.is_current());
        manager
            .switch_to(&a_account.id)
            .await
            .expect("Cannot switch back");
        let renewed = manager
            .account_selection(&a_account.id)
            .expect("Renewed selection missing");
        // A late preparation under the old selection is refused at entry.
        assert!(matches!(
            live.prepare_pending_send(&selection, &scope, &text, revision)
                .await,
            Err(PrepareError::Store(MatrixCoreError::SelectedAccountChanged))
        ));
        // The stale continuation's rollback (its grant having failed with the
        // selection) touches nothing committed and writes nothing.
        let dirty_before = live.drafts.dirty_keys();
        {
            let tx = live.drafts.begin_write_transaction().await;
            live.rollback_pending(
                &tx,
                &selection,
                &scope,
                &nonce,
                &Prepared {
                    record: true,
                    index_added: false,
                    index_committed: false,
                },
            )
            .await;
        }
        assert_eq!(
            status(),
            Some(PendingStatus::Enqueuing),
            "the departed cache is left alone"
        );
        assert_eq!(
            live.drafts.dirty_keys(),
            dirty_before,
            "no write was scheduled"
        );
        assert_eq!(
            in_row().await,
            Some(false),
            "the committed record is preserved for discovery"
        );
        // The same for a thread send prepared before the switch: its room
        // index entry (added and committed by that preparation) and its
        // committed record both stay; a stale rollback removes neither.
        let root = "$interrupted-thread:fixture.invalid";
        let thread_scope = SendScope {
            account: a_account.id.clone(),
            room: a_room.clone(),
            thread: Some(root.to_owned()),
        };
        {
            let tx = live.drafts.begin_write_transaction().await;
            live.rollback_pending(
                &tx,
                &selection,
                &thread_scope,
                &thread_nonce,
                &Prepared {
                    record: true,
                    index_added: true,
                    index_committed: true,
                },
            )
            .await;
        }
        assert!(
            live.drafts
                .thread_send_pending(&a_account.id, &a_room, root),
            "the thread record stays cached"
        );
        let room_row = manager
            .load_composed_draft_for(&a_account.id, &a_room, None)
            .await
            .unwrap();
        assert!(
            room_row
                .envelope
                .threads()
                .iter()
                .any(|known| known == root),
            "the durable index anchor stays"
        );
        assert_eq!(
            manager
                .load_composed_draft_for(&a_account.id, &a_room, Some(root))
                .await
                .unwrap()
                .envelope
                .find(&thread_nonce)
                .map(|record| record.is_queued()),
            Some(false),
            "the thread's committed record stays for discovery"
        );
        assert_eq!(
            live.drafts.dirty_keys(),
            dirty_before,
            "still no write scheduled"
        );
        // The renewed selection re-seeds the scope: the id-less record is now
        // an unconfirmed record awaiting the user's review, the row is still
        // writable, and the account-bound flush does not get stuck.
        live.adopt_recovered_sends(renewed.clone(), Weak::default())
            .await;
        assert_eq!(status(), Some(PendingStatus::Unconfirmed));
        assert_eq!(in_row().await, Some(false));
        live.drafts
            .set(&a_account.id, &a_room, "typed after the switch");
        live.flush_drafts()
            .await
            .expect("the cache flushes without dropping the restored record");
        assert_eq!(
            in_row().await,
            Some(false),
            "a flush keeps the restored record"
        );
        assert!(live.drafts.dirty_keys().is_empty());
        // Nothing of ours may replay it under the new selection.
        assert!(matches!(
            manager
                .grant_enqueue_for(&renewed, &a_room, None, &nonce)
                .await,
            Err(MatrixCoreError::PendingSendUnknown)
        ));

        manager
            .logout_account(&a_account.id)
            .await
            .expect("Cannot remove first account");
        manager
            .logout_account(&b_account.id)
            .await
            .expect("Cannot remove second account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
        recovery_check!(interrupted_prepare);
    }

    /// Revocation racing an owned rollback. The rollback of a committed
    /// pre-enqueue record runs concurrently with the account switch (and the
    /// switch back) rather than after it; wherever the revocation lands,
    /// cache and row must agree afterwards (record kept in both for review, or
    /// removed from both under the exact release), the account-bound flush
    /// must still succeed, and nothing is left dirty. The interleaving point is
    /// not injected, so this is a race over the real awaits, not a fixed one.
    #[tokio::test]
    async fn rollback_racing_a_switch_leaves_cache_and_row_agreeing() {
        let Some((alice, carol, a_room)) = fixture() else {
            eprintln!(
                "Skipped rollback race regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
            );
            return;
        };
        recovery_phase!(rollback_race);
        let directory = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(directory.path().join("profile")).unwrap());
        let voices = Arc::new(VoiceSpaces::new(morpheus_config::ConfigStore::at(
            directory.path().join("config.toml"),
        )));
        let live = LiveUi::new(manager.clone(), Handle::current(), Weak::default(), voices);
        let a_account = manager
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("First login failed");
        wait_for_room(&manager, &a_room).await;
        let selection = manager
            .account_selection(&a_account.id)
            .expect("Selection missing");
        manager.take_recovered_drafts(&selection).await.unwrap();
        live.is_live.store(true, Ordering::Release);
        *live.selected_room.lock().await = Some(a_room.clone());
        let scope = SendScope {
            account: a_account.id.clone(),
            room: a_room.clone(),
            thread: None,
        };
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let text = ComposeText::plain(format!("rollback-race-{unique}"));
        live.drafts.set(&a_account.id, &a_room, &text.body);
        let revision = live.drafts.revision(&a_account.id, &a_room).unwrap();
        let handle = live
            .prepare_pending_send(&selection, &scope, &text, revision)
            .await
            .expect("the record is durable before enqueue");
        let nonce = handle.nonce().clone();
        let in_row = || async {
            manager
                .load_composed_draft_for(&a_account.id, &a_room, None)
                .await
                .unwrap()
                .envelope
                .find(&nonce)
                .is_some()
        };
        assert!(in_row().await);
        // The owned rollback of the committed record starts now and races the
        // switch that revokes its selection.
        let racing_live = live.clone();
        let racing_selection = selection.clone();
        let racing_scope = scope.clone();
        let racing_nonce = nonce.clone();
        let rollback = tokio::spawn(async move {
            let tx = racing_live.drafts.begin_write_transaction().await;
            racing_live
                .rollback_pending(
                    &tx,
                    &racing_selection,
                    &racing_scope,
                    &racing_nonce,
                    &Prepared {
                        record: true,
                        index_added: false,
                        index_committed: false,
                    },
                )
                .await;
        });
        let b_account = manager
            .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
            .await
            .expect("Second login failed");
        manager
            .switch_to(&a_account.id)
            .await
            .expect("Cannot switch back");
        tokio::time::timeout(Duration::from_secs(30), rollback)
            .await
            .expect("the rollback must finish")
            .unwrap();
        let renewed = manager
            .account_selection(&a_account.id)
            .expect("Renewed selection missing");
        // Whatever the interleaving, cache and row agree.
        let cached = live
            .drafts
            .record_status(&a_account.id, &a_room, None, &nonce)
            .is_some();
        assert_eq!(
            cached,
            in_row().await,
            "cache and row must agree on the record after the race"
        );
        // Re-seeding and flushing under the renewed selection succeed and keep
        // that agreement; a surviving record is an unconfirmed restored one.
        live.adopt_recovered_sends(renewed.clone(), Weak::default())
            .await;
        live.flush_drafts()
            .await
            .expect("the account-bound flush must succeed after the race");
        assert!(live.drafts.dirty_keys().is_empty(), "nothing stays dirty");
        let cached_after = live
            .drafts
            .record_status(&a_account.id, &a_room, None, &nonce);
        assert_eq!(cached_after.is_some(), in_row().await);
        if let Some(status) = cached_after {
            assert_eq!(status, PendingStatus::Unconfirmed);
        }

        manager
            .logout_account(&a_account.id)
            .await
            .expect("Cannot remove first account");
        manager
            .logout_account(&b_account.id)
            .await
            .expect("Cannot remove second account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
        recovery_check!(rollback_race);
    }

    /// Shared setup for the forced-interleaving tests: one logged-in account,
    /// discovery complete, the room selected.
    async fn prepared_live(
        alice: &FixtureAccount,
        a_room: &str,
    ) -> (
        tempfile::TempDir,
        Arc<AccountManager>,
        Arc<LiveUi>,
        morpheus_matrix::AccountInfo,
        AccountSelection,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(directory.path().join("profile")).unwrap());
        let voices = Arc::new(VoiceSpaces::new(morpheus_config::ConfigStore::at(
            directory.path().join("config.toml"),
        )));
        let live = LiveUi::new(manager.clone(), Handle::current(), Weak::default(), voices);
        let a_account = manager
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("First login failed");
        wait_for_room(&manager, a_room).await;
        let selection = manager
            .account_selection(&a_account.id)
            .expect("Selection missing");
        manager.take_recovered_drafts(&selection).await.unwrap();
        live.is_live.store(true, Ordering::Release);
        *live.selected_room.lock().await = Some(a_room.to_owned());
        (directory, manager, live, a_account, selection)
    }

    fn unique_text(prefix: &str) -> ComposeText {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        ComposeText::plain(format!("{prefix}-{unique}"))
    }

    async fn row_has_record(
        manager: &AccountManager,
        account: &str,
        room: &str,
        thread: Option<&str>,
        nonce: &Generation,
    ) -> bool {
        manager
            .load_composed_draft_for(account, room, thread)
            .await
            .unwrap()
            .envelope
            .find(nonce)
            .is_some()
    }

    /// Forced interleaving: the background flush (the writer's path) runs
    /// while a preparation has created its record in the cache but not yet
    /// written it. The flush must wait for the preparation's transaction, and
    /// the record must reach the store only through the preparation's own
    /// write, so a failed preparation write would prove the record absent.
    #[tokio::test]
    async fn a_background_flush_cannot_commit_a_record_before_its_preparation_writes_it() {
        let Some((alice, _carol, a_room)) = fixture() else {
            eprintln!(
                "Skipped flush ordering regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
            );
            return;
        };
        recovery_phase!(writer_transaction);
        let (directory, manager, live, a_account, selection) = prepared_live(&alice, &a_room).await;
        let scope = SendScope {
            account: a_account.id.clone(),
            room: a_room.clone(),
            thread: None,
        };
        let text = unique_text("flush-ordering");
        live.drafts.set(&a_account.id, &a_room, &text.body);
        let revision = live.drafts.revision(&a_account.id, &a_room).unwrap();
        let point = live.pauses.install("prepare.after_begin");
        let (prep_live, prep_selection, prep_scope, prep_text) =
            (live.clone(), selection.clone(), scope.clone(), text.clone());
        let prepare = tokio::spawn(async move {
            prep_live
                .prepare_pending_send(&prep_selection, &prep_scope, &prep_text, revision)
                .await
        });
        point.entered().await;
        // The record exists only in the cache, dirty, with the writer lock
        // held by the preparation.
        let nonce = live
            .drafts
            .pending_items(&a_account.id, &a_room, None)
            .pop()
            .expect("the record is in the cache")
            .nonce;
        assert!(
            live.drafts
                .dirty_keys()
                .contains(&(a_account.id.clone(), a_room.clone(), None))
        );
        assert!(
            !row_has_record(&manager, &a_account.id, &a_room, None, &nonce).await,
            "nothing is in the store yet"
        );
        // Poll the production flush once: the dirty scan is synchronous, so
        // its only await is the writer lock the preparation holds.
        let mut flush = Box::pin(live.flush_drafts());
        assert!(
            futures_util::poll!(flush.as_mut()).is_pending(),
            "the flush waits on the preparation's writer transaction"
        );
        assert!(
            !row_has_record(&manager, &a_account.id, &a_room, None, &nonce).await,
            "the flush could not commit the record first"
        );
        point.proceed();
        let handle = tokio::time::timeout(Duration::from_secs(30), prepare)
            .await
            .expect("the preparation must finish")
            .unwrap()
            .expect("the preparation succeeds once released");
        assert_eq!(handle.nonce(), &nonce);
        tokio::time::timeout(Duration::from_secs(30), flush)
            .await
            .expect("the flush must finish")
            .expect("the flush succeeds after the transaction");
        assert!(row_has_record(&manager, &a_account.id, &a_room, None, &nonce).await);

        manager
            .logout_account(&a_account.id)
            .await
            .expect("Cannot remove the account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
        recovery_check!(writer_transaction);
    }

    /// Forced interleaving: a thread preparation paused right after the room
    /// index became durable. The row holds the index, the thread row holds no
    /// record and the cache holds no record; the record appears only after.
    #[tokio::test]
    async fn the_thread_index_is_durable_before_any_record_exists() {
        let Some((alice, _carol, a_room)) = fixture() else {
            eprintln!(
                "Skipped index ordering regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
            );
            return;
        };
        recovery_phase!(thread_index);
        let (directory, manager, live, a_account, selection) = prepared_live(&alice, &a_room).await;
        let root = "$index-first:fixture.invalid";
        let scope = SendScope {
            account: a_account.id.clone(),
            room: a_room.clone(),
            thread: Some(root.to_owned()),
        };
        live.drafts
            .set_scoped(&a_account.id, &a_room, Some(root), "thread reply");
        let revision = live
            .drafts
            .scoped_revision(&a_account.id, &a_room, Some(root))
            .unwrap();
        let point = live.pauses.install("prepare.after_index_commit");
        let (prep_live, prep_selection, prep_scope) =
            (live.clone(), selection.clone(), scope.clone());
        let prepare = tokio::spawn(async move {
            prep_live
                .prepare_pending_send(
                    &prep_selection,
                    &prep_scope,
                    &ComposeText::plain("thread reply"),
                    revision,
                )
                .await
        });
        point.entered().await;
        let room_row = manager
            .load_composed_draft_for(&a_account.id, &a_room, None)
            .await
            .unwrap();
        assert!(
            room_row
                .envelope
                .threads()
                .iter()
                .any(|known| known == root),
            "the index is durable first"
        );
        let thread_row = manager
            .load_composed_draft_for(&a_account.id, &a_room, Some(root))
            .await
            .unwrap();
        assert!(
            thread_row.envelope.pending().is_empty(),
            "no record is in the store yet"
        );
        assert!(
            !live
                .drafts
                .thread_send_pending(&a_account.id, &a_room, root),
            "no record is in the cache yet"
        );
        point.proceed();
        let handle = tokio::time::timeout(Duration::from_secs(30), prepare)
            .await
            .expect("the preparation must finish")
            .unwrap()
            .expect("the preparation succeeds once released");
        assert!(row_has_record(&manager, &a_account.id, &a_room, Some(root), handle.nonce()).await);
        assert!(
            live.drafts
                .thread_send_pending(&a_account.id, &a_room, root)
        );

        manager
            .logout_account(&a_account.id)
            .await
            .expect("Cannot remove the account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
        recovery_check!(thread_index);
    }

    /// Forced interleaving: the selection is revoked after the core granted
    /// the rollback's exact release but before the cache was touched. Cache
    /// and row both keep the record; the renewed selection sees it as
    /// unconfirmed and the flush still succeeds.
    #[tokio::test]
    async fn revocation_after_the_release_leaves_the_record_in_cache_and_row() {
        let Some((alice, carol, a_room)) = fixture() else {
            eprintln!(
                "Skipped release ordering regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
            );
            return;
        };
        recovery_phase!(release_revocation);
        let (directory, manager, live, a_account, selection) = prepared_live(&alice, &a_room).await;
        let scope = SendScope {
            account: a_account.id.clone(),
            room: a_room.clone(),
            thread: None,
        };
        let text = unique_text("release-ordering");
        live.drafts.set(&a_account.id, &a_room, &text.body);
        let revision = live.drafts.revision(&a_account.id, &a_room).unwrap();
        let handle = live
            .prepare_pending_send(&selection, &scope, &text, revision)
            .await
            .expect("the record is durable before enqueue");
        let nonce = handle.nonce().clone();
        let point = live.pauses.install("rollback.after_release");
        let (rb_live, rb_selection, rb_scope, rb_nonce) = (
            live.clone(),
            selection.clone(),
            scope.clone(),
            nonce.clone(),
        );
        let rollback = tokio::spawn(async move {
            let tx = rb_live.drafts.begin_write_transaction().await;
            rb_live
                .rollback_pending(
                    &tx,
                    &rb_selection,
                    &rb_scope,
                    &rb_nonce,
                    &Prepared {
                        record: true,
                        index_added: false,
                        index_committed: false,
                    },
                )
                .await;
        });
        point.entered().await;
        let b_account = manager
            .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
            .await
            .expect("Second login failed");
        assert!(!selection.is_current());
        point.proceed();
        tokio::time::timeout(Duration::from_secs(30), rollback)
            .await
            .expect("the rollback must finish")
            .unwrap();
        assert_eq!(
            live.drafts
                .record_status(&a_account.id, &a_room, None, &nonce),
            Some(PendingStatus::Enqueuing),
            "the cache was not touched after the revocation"
        );
        assert!(
            row_has_record(&manager, &a_account.id, &a_room, None, &nonce).await,
            "the row keeps the record"
        );
        manager
            .switch_to(&a_account.id)
            .await
            .expect("Cannot switch back");
        let renewed = manager.account_selection(&a_account.id).unwrap();
        live.adopt_recovered_sends(renewed, Weak::default()).await;
        assert_eq!(
            live.drafts
                .record_status(&a_account.id, &a_room, None, &nonce),
            Some(PendingStatus::Unconfirmed)
        );
        live.flush_drafts().await.expect("the flush succeeds");
        assert!(row_has_record(&manager, &a_account.id, &a_room, None, &nonce).await);
        assert!(live.drafts.dirty_keys().is_empty());

        manager
            .logout_account(&a_account.id)
            .await
            .expect("Cannot remove first account");
        manager
            .logout_account(&b_account.id)
            .await
            .expect("Cannot remove second account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
        recovery_check!(release_revocation);
    }

    /// Forced interleaving: the selection is revoked after the rollback
    /// removed the released record from the cache but before its write. The
    /// write still completes under the exact release (fresh ownership no
    /// longer matters), so cache and row agree on the record being gone.
    #[tokio::test]
    async fn revocation_during_the_rollback_write_converges_under_the_release() {
        let Some((alice, carol, a_room)) = fixture() else {
            eprintln!(
                "Skipped rollback write regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
            );
            return;
        };
        recovery_phase!(write_revocation);
        let (directory, manager, live, a_account, selection) = prepared_live(&alice, &a_room).await;
        let scope = SendScope {
            account: a_account.id.clone(),
            room: a_room.clone(),
            thread: None,
        };
        let text = unique_text("rollback-write");
        live.drafts.set(&a_account.id, &a_room, &text.body);
        let revision = live.drafts.revision(&a_account.id, &a_room).unwrap();
        let handle = live
            .prepare_pending_send(&selection, &scope, &text, revision)
            .await
            .expect("the record is durable before enqueue");
        let nonce = handle.nonce().clone();
        let point = live.pauses.install("rollback.before_write");
        let (rb_live, rb_selection, rb_scope, rb_nonce) = (
            live.clone(),
            selection.clone(),
            scope.clone(),
            nonce.clone(),
        );
        let rollback = tokio::spawn(async move {
            let tx = rb_live.drafts.begin_write_transaction().await;
            rb_live
                .rollback_pending(
                    &tx,
                    &rb_selection,
                    &rb_scope,
                    &rb_nonce,
                    &Prepared {
                        record: true,
                        index_added: false,
                        index_committed: false,
                    },
                )
                .await;
        });
        point.entered().await;
        assert!(
            live.drafts
                .record_status(&a_account.id, &a_room, None, &nonce)
                .is_none(),
            "released and removed from the cache"
        );
        assert!(
            row_has_record(&manager, &a_account.id, &a_room, None, &nonce).await,
            "not yet written"
        );
        let b_account = manager
            .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
            .await
            .expect("Second login failed");
        assert!(!selection.is_current());
        point.proceed();
        tokio::time::timeout(Duration::from_secs(30), rollback)
            .await
            .expect("the rollback must finish")
            .unwrap();
        assert!(
            !row_has_record(&manager, &a_account.id, &a_room, None, &nonce).await,
            "the release authorized the write"
        );
        assert!(
            live.drafts
                .record_status(&a_account.id, &a_room, None, &nonce)
                .is_none()
        );
        manager
            .switch_to(&a_account.id)
            .await
            .expect("Cannot switch back");
        let renewed = manager.account_selection(&a_account.id).unwrap();
        live.adopt_recovered_sends(renewed, Weak::default()).await;
        live.flush_drafts().await.expect("the flush succeeds");
        assert!(live.drafts.dirty_keys().is_empty());
        assert!(!row_has_record(&manager, &a_account.id, &a_room, None, &nonce).await);

        manager
            .logout_account(&a_account.id)
            .await
            .expect("Cannot remove first account");
        manager
            .logout_account(&b_account.id)
            .await
            .expect("Cannot remove second account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
        recovery_check!(write_revocation);
    }

    /// Forced interleaving: a preparation queued behind the writer lock (as
    /// behind the background flush) whose selection ends while it waits. Once
    /// the lock is released it creates nothing: no record, no index entry, no
    /// dirty scope, no grant, no row change.
    #[tokio::test]
    async fn a_preparation_waiting_for_the_writer_lock_creates_nothing_once_its_selection_ended() {
        let Some((alice, carol, a_room)) = fixture() else {
            eprintln!(
                "Skipped writer-lock wait regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
            );
            return;
        };
        recovery_phase!(writer_lock_revocation);
        let (directory, manager, live, a_account, selection) = prepared_live(&alice, &a_room).await;
        let root = "$behind-the-lock:fixture.invalid";
        let scope = SendScope {
            account: a_account.id.clone(),
            room: a_room.clone(),
            thread: Some(root.to_owned()),
        };
        live.drafts.set(&a_account.id, &a_room, "room text stays");
        live.drafts
            .set_scoped(&a_account.id, &a_room, Some(root), "thread reply");
        live.flush_drafts().await.unwrap();
        let revision = live
            .drafts
            .scoped_revision(&a_account.id, &a_room, Some(root))
            .unwrap();
        let dirty_before = live.drafts.dirty_keys();
        // The writer lock is held, as the background flush would hold it.
        // Poll the production preparation once: the room is already cached
        // by the set and flush above, so its load completes synchronously
        // and the only thing it can wait on is the held writer lock, which
        // it reaches only after passing its initial selection check.
        let held = live.drafts.begin_write_transaction().await;
        let text = ComposeText::plain("thread reply");
        let mut prepare = Box::pin(live.prepare_pending_send(&selection, &scope, &text, revision));
        assert!(
            futures_util::poll!(prepare.as_mut()).is_pending(),
            "the preparation waits for the writer lock"
        );
        // A -> B -> A while it waits.
        let b_account = manager
            .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
            .await
            .expect("Second login failed");
        manager
            .switch_to(&a_account.id)
            .await
            .expect("Cannot switch back");
        assert!(!selection.is_current());
        drop(held);
        // The same future resumes with the lock and must create nothing.
        let outcome = tokio::time::timeout(Duration::from_secs(30), prepare)
            .await
            .expect("the preparation must finish");
        assert!(matches!(
            outcome,
            Err(PrepareError::Store(MatrixCoreError::SelectedAccountChanged))
        ));
        assert!(
            live.drafts
                .pending_items(&a_account.id, &a_room, Some(root))
                .is_empty(),
            "no record was created"
        );
        assert!(
            !live
                .drafts
                .thread_send_pending(&a_account.id, &a_room, root)
        );
        assert_eq!(
            live.drafts.dirty_keys(),
            dirty_before,
            "no scope was dirtied"
        );
        let room_row = manager
            .load_composed_draft_for(&a_account.id, &a_room, None)
            .await
            .unwrap();
        assert!(
            !room_row
                .envelope
                .threads()
                .iter()
                .any(|known| known == root),
            "no index entry was written"
        );
        assert_eq!(room_row.text.body, "room text stays");
        let thread_row = manager
            .load_composed_draft_for(&a_account.id, &a_room, Some(root))
            .await
            .unwrap();
        assert!(
            thread_row.envelope.pending().is_empty(),
            "no record was written"
        );

        manager
            .logout_account(&a_account.id)
            .await
            .expect("Cannot remove first account");
        manager
            .logout_account(&b_account.id)
            .await
            .expect("Cannot remove second account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
        recovery_check!(writer_lock_revocation);
    }

    fn typed_with_mention(prefix: &str, user_id: &str) -> ComposeText {
        let body = format!("{prefix} {user_id} **soon**");
        let start = body.find(user_id).unwrap();
        ComposeText::new(body, TextFormat::Markdown)
            .insert_mention(
                start..start + user_id.len(),
                morpheus_matrix::MentionTarget::User(user_id.to_owned()),
            )
            .expect("Cannot encode the mention")
            .0
    }

    /// Forced interleaving: the user keeps typing while a preparation awaits,
    /// then the send fails definitely before enqueue. The submitted text, with
    /// its format and mention, is kept beside the newer composer text through
    /// the core's exact release; the newer text is untouched. With the
    /// composer unchanged the record is simply dropped and nothing is
    /// duplicated.
    #[tokio::test]
    async fn a_submission_typed_over_during_preparation_is_kept_on_a_definite_failure() {
        let Some((alice, _carol, a_room)) = fixture() else {
            eprintln!("Skipped retention regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in.");
            return;
        };
        recovery_phase!(typed_retention);
        let (directory, manager, live, a_account, selection) = prepared_live(&alice, &a_room).await;
        let scope = SendScope {
            account: a_account.id.clone(),
            room: a_room.clone(),
            thread: None,
        };
        let submitted = typed_with_mention("mention", &a_account.user_id);
        live.drafts
            .set_composed_scoped(&a_account.id, &a_room, None, submitted.clone())
            .unwrap();
        let revision = live.drafts.revision(&a_account.id, &a_room).unwrap();
        let point = live.pauses.install("prepare.after_begin");
        let (prep_live, prep_selection, prep_scope, prep_text) = (
            live.clone(),
            selection.clone(),
            scope.clone(),
            submitted.clone(),
        );
        let prepare = tokio::spawn(async move {
            prep_live
                .prepare_pending_send(&prep_selection, &prep_scope, &prep_text, revision)
                .await
        });
        point.entered().await;
        // Typing continues while the preparation awaits its write.
        live.drafts
            .set(&a_account.id, &a_room, "typed over it meanwhile");
        point.proceed();
        let handle = tokio::time::timeout(Duration::from_secs(30), prepare)
            .await
            .expect("the preparation must finish")
            .unwrap()
            .expect("the preparation succeeds");
        let nonce = handle.nonce().clone();
        // A definite pre-enqueue failure: the controller abandons the send.
        live.abandon_pending_send(&selection, &scope, &nonce).await;
        assert_eq!(
            live.drafts
                .record_status(&a_account.id, &a_room, None, &nonce),
            Some(PendingStatus::Kept)
        );
        assert_eq!(
            live.drafts
                .record_text(&a_account.id, &a_room, None, &nonce),
            Some(submitted.clone()),
            "the exact typed submission, format and mention, is kept"
        );
        assert_eq!(
            live.drafts.get(&a_account.id, &a_room),
            "typed over it meanwhile",
            "the newer text is untouched"
        );
        let row = manager
            .load_composed_draft_for(&a_account.id, &a_room, None)
            .await
            .unwrap();
        let record = row
            .envelope
            .find(&nonce)
            .expect("the kept record is in the row");
        assert!(record.is_kept());
        assert_eq!(record.text().to_compose().unwrap(), submitted);
        assert_eq!(row.text.body, "typed over it meanwhile");
        live.flush_drafts().await.unwrap();
        assert!(live.drafts.dirty_keys().is_empty());
        // Unchanged composer: the record is dropped, the text stays once.
        live.drafts
            .set(&a_account.id, &a_room, "unchanged submission");
        let unchanged = live
            .drafts
            .get_composed_scoped(&a_account.id, &a_room, None);
        assert_eq!(unchanged.body, "unchanged submission");
        assert_eq!(unchanged.format, TextFormat::Markdown);
        assert!(
            unchanged.mentions.is_empty(),
            "the old mention was not retained"
        );
        let revision = live.drafts.revision(&a_account.id, &a_room).unwrap();
        let again = live
            .prepare_pending_send(&selection, &scope, &unchanged, revision)
            .await
            .expect("the second preparation succeeds");
        let second = again.nonce().clone();
        live.abandon_pending_send(&selection, &scope, &second).await;
        assert!(
            live.drafts
                .record_status(&a_account.id, &a_room, None, &second)
                .is_none(),
            "dropped, not kept"
        );
        assert_eq!(
            live.drafts.get(&a_account.id, &a_room),
            "unchanged submission"
        );
        let row = manager
            .load_composed_draft_for(&a_account.id, &a_room, None)
            .await
            .unwrap();
        assert_eq!(row.text, unchanged);
        assert_eq!(
            live.drafts
                .get_composed_scoped(&a_account.id, &a_room, None),
            unchanged,
            "abandoning the unchanged submission preserves its complete typed text"
        );
        assert!(row.envelope.find(&second).is_none());
        assert!(
            row.envelope
                .find(&nonce)
                .is_some_and(|record| record.is_kept()),
            "the earlier kept text remains"
        );

        manager
            .logout_account(&a_account.id)
            .await
            .expect("Cannot remove the account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
        recovery_check!(typed_retention);
    }

    /// Forced interleaving: the composer is edited while the rollback waits
    /// after the core's release, past any earlier comparison. The decision is
    /// made against the composer text at the mutation, so the submission is
    /// kept rather than dropped.
    #[tokio::test]
    async fn typing_while_the_rollback_waits_after_the_release_keeps_the_submission() {
        let Some((alice, _carol, a_room)) = fixture() else {
            eprintln!(
                "Skipped release-wait retention regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
            );
            return;
        };
        recovery_phase!(late_typing_retention);
        let (directory, manager, live, a_account, selection) = prepared_live(&alice, &a_room).await;
        let scope = SendScope {
            account: a_account.id.clone(),
            room: a_room.clone(),
            thread: None,
        };
        let submitted = typed_with_mention("late mention", &a_account.user_id);
        live.drafts
            .set_composed_scoped(&a_account.id, &a_room, None, submitted.clone())
            .unwrap();
        let revision = live.drafts.revision(&a_account.id, &a_room).unwrap();
        let handle = live
            .prepare_pending_send(&selection, &scope, &submitted, revision)
            .await
            .expect("the preparation succeeds");
        let nonce = handle.nonce().clone();
        // The composer is unchanged when the rollback starts.
        let point = live.pauses.install("rollback.after_release");
        let (back_live, back_selection, back_scope, back_nonce) = (
            live.clone(),
            selection.clone(),
            scope.clone(),
            nonce.clone(),
        );
        let rollback = tokio::spawn(async move {
            back_live
                .abandon_pending_send(&back_selection, &back_scope, &back_nonce)
                .await
        });
        point.entered().await;
        // Typing continues while the released rollback waits.
        live.drafts
            .set(&a_account.id, &a_room, "typed after the release");
        point.proceed();
        tokio::time::timeout(Duration::from_secs(30), rollback)
            .await
            .expect("the rollback must finish")
            .unwrap();
        assert_eq!(
            live.drafts
                .record_status(&a_account.id, &a_room, None, &nonce),
            Some(PendingStatus::Kept),
            "decided at the mutation, not before the release"
        );
        assert_eq!(
            live.drafts
                .record_text(&a_account.id, &a_room, None, &nonce),
            Some(submitted.clone())
        );
        assert_eq!(
            live.drafts.get(&a_account.id, &a_room),
            "typed after the release"
        );
        let row = manager
            .load_composed_draft_for(&a_account.id, &a_room, None)
            .await
            .unwrap();
        assert!(
            row.envelope
                .find(&nonce)
                .is_some_and(|record| record.is_kept())
        );
        assert_eq!(row.text.body, "typed after the release");
        live.flush_drafts().await.unwrap();
        assert!(live.drafts.dirty_keys().is_empty());

        manager
            .logout_account(&a_account.id)
            .await
            .expect("Cannot remove the account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
        recovery_check!(late_typing_retention);
    }

    /// A rollback of a record that never reached the row, run under a
    /// selection that has ended, deletes nothing the composer no longer
    /// holds: the cache keeps the enqueuing record with its exact text (no
    /// grant, no write, no index change), and the account-bound flush later
    /// writes it as the enqueuing record it is. A record the composer still
    /// holds exactly is dropped, as before.
    #[tokio::test]
    async fn a_revoked_rollback_retains_a_typed_over_uncommitted_record() {
        let Some((alice, carol, a_room)) = fixture() else {
            eprintln!(
                "Skipped revoked retention regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
            );
            return;
        };
        recovery_phase!(revoked_retention);
        let (directory, manager, live, a_account, selection) = prepared_live(&alice, &a_room).await;
        let scope = SendScope {
            account: a_account.id.clone(),
            room: a_room.clone(),
            thread: None,
        };
        let submitted = typed_with_mention("revoked mention", &a_account.user_id);
        live.drafts
            .set_composed_scoped(&a_account.id, &a_room, None, submitted.clone())
            .unwrap();
        live.flush_drafts().await.unwrap();
        // Two cache-only records, as a preparation holds before its write.
        let revision = live.drafts.revision(&a_account.id, &a_room).unwrap();
        let typed_over = live
            .drafts
            .begin_pending(&a_account.id, &a_room, None, &submitted, revision)
            .expect("the record is created in the cache");
        let held = ComposeText::plain("still in the composer");
        live.drafts
            .set_composed_scoped(&a_account.id, &a_room, None, held.clone())
            .unwrap();
        let revision = live.drafts.revision(&a_account.id, &a_room).unwrap();
        let still_held = live
            .drafts
            .begin_pending(&a_account.id, &a_room, None, &held, revision)
            .expect("the second record is created in the cache");
        // A -> B -> A while both exist only in the cache.
        let b_account = manager
            .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
            .await
            .expect("Second login failed");
        manager
            .switch_to(&a_account.id)
            .await
            .expect("Cannot switch back");
        assert!(!selection.is_current());
        {
            let tx = live.drafts.begin_write_transaction().await;
            live.rollback_pending(
                &tx,
                &selection,
                &scope,
                typed_over.nonce(),
                &Prepared::default(),
            )
            .await;
            live.rollback_pending(
                &tx,
                &selection,
                &scope,
                still_held.nonce(),
                &Prepared::default(),
            )
            .await;
        }
        assert_eq!(
            live.drafts
                .record_status(&a_account.id, &a_room, None, typed_over.nonce()),
            Some(PendingStatus::Enqueuing),
            "nothing absent from the composer was deleted"
        );
        assert_eq!(
            live.drafts
                .record_text(&a_account.id, &a_room, None, typed_over.nonce()),
            Some(submitted.clone())
        );
        assert!(
            live.drafts
                .record_status(&a_account.id, &a_room, None, still_held.nonce())
                .is_none(),
            "the submission the composer holds is dropped"
        );
        assert_eq!(live.drafts.get(&a_account.id, &a_room), held.body);
        assert!(
            live.drafts
                .dirty_keys()
                .contains(&(a_account.id.clone(), a_room.clone(), None)),
            "left for the account-bound flush"
        );
        let row = manager
            .load_composed_draft_for(&a_account.id, &a_room, None)
            .await
            .unwrap();
        assert!(
            row.envelope.pending().is_empty(),
            "the revoked rollbacks wrote nothing"
        );
        assert_eq!(row.text.body, submitted.body);
        // The account-bound flush under the renewed selection writes the
        // retained record as the enqueuing record it is.
        live.flush_drafts()
            .await
            .expect("the flush writes the retained record");
        let row = manager
            .load_composed_draft_for(&a_account.id, &a_room, None)
            .await
            .unwrap();
        let record = row
            .envelope
            .find(typed_over.nonce())
            .expect("retained for discovery");
        assert!(!record.is_queued() && !record.is_kept());
        assert_eq!(record.text().to_compose().unwrap(), submitted);
        assert!(row.envelope.find(still_held.nonce()).is_none());
        assert_eq!(row.text.body, held.body);

        manager
            .logout_account(&a_account.id)
            .await
            .expect("Cannot remove first account");
        manager
            .logout_account(&b_account.id)
            .await
            .expect("Cannot remove second account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
        recovery_check!(revoked_retention);
    }

    /// The same for a thread: the kept record keeps its room index entry, so
    /// discovery still finds it.
    #[tokio::test]
    async fn a_thread_submission_typed_over_during_preparation_is_kept_with_its_index() {
        let Some((alice, _carol, a_room)) = fixture() else {
            eprintln!(
                "Skipped thread retention regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
            );
            return;
        };
        recovery_phase!(thread_retention);
        let (directory, manager, live, a_account, selection) = prepared_live(&alice, &a_room).await;
        let root = "$kept-thread:fixture.invalid";
        let scope = SendScope {
            account: a_account.id.clone(),
            room: a_room.clone(),
            thread: Some(root.to_owned()),
        };
        let submitted = typed_with_mention("thread mention", &a_account.user_id);
        live.drafts
            .set_composed_scoped(&a_account.id, &a_room, Some(root), submitted.clone())
            .unwrap();
        let revision = live
            .drafts
            .scoped_revision(&a_account.id, &a_room, Some(root))
            .unwrap();
        let point = live.pauses.install("prepare.after_begin");
        let (prep_live, prep_selection, prep_scope, prep_text) = (
            live.clone(),
            selection.clone(),
            scope.clone(),
            submitted.clone(),
        );
        let prepare = tokio::spawn(async move {
            prep_live
                .prepare_pending_send(&prep_selection, &prep_scope, &prep_text, revision)
                .await
        });
        point.entered().await;
        live.drafts
            .set_scoped(&a_account.id, &a_room, Some(root), "newer thread text");
        point.proceed();
        let handle = tokio::time::timeout(Duration::from_secs(30), prepare)
            .await
            .expect("the preparation must finish")
            .unwrap()
            .expect("the preparation succeeds");
        let nonce = handle.nonce().clone();
        live.abandon_pending_send(&selection, &scope, &nonce).await;
        assert_eq!(
            live.drafts
                .record_status(&a_account.id, &a_room, Some(root), &nonce),
            Some(PendingStatus::Kept)
        );
        assert_eq!(
            live.drafts
                .record_text(&a_account.id, &a_room, Some(root), &nonce),
            Some(submitted.clone())
        );
        let thread_row = manager
            .load_composed_draft_for(&a_account.id, &a_room, Some(root))
            .await
            .unwrap();
        assert!(
            thread_row
                .envelope
                .find(&nonce)
                .is_some_and(|record| record.is_kept())
        );
        assert_eq!(thread_row.text.body, "newer thread text");
        let room_row = manager
            .load_composed_draft_for(&a_account.id, &a_room, None)
            .await
            .unwrap();
        assert!(
            room_row
                .envelope
                .threads()
                .iter()
                .any(|known| known == root),
            "a thread with kept text keeps its index entry"
        );
        live.flush_drafts().await.unwrap();
        assert!(live.drafts.dirty_keys().is_empty());

        manager
            .logout_account(&a_account.id)
            .await
            .expect("Cannot remove the account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
        recovery_check!(thread_retention);
    }
}
