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
    AccountInfo, AccountManager, AccountSelection, AnswerFence, ComposeSendTarget, ComposeText,
    CreatedRoom, CreationError, CreationKind, CreationPlan, DownloadedAttachment, EditBaseline,
    Generation, InvitationAnswer, InvitationOutcome, JoinCancellation, JoinError, MatrixCoreError,
    MatrixLinkTarget, MessageInfo, MessageSendState, Placement, PublicMediaConsent,
    QueuedAttachment, QueuedMessage, QueuedSendUpdate, RecoveredDraft, RoomInfo, RoomMemberInfo,
    RoomMembership, StickerCatalog, SyncBackend, SyncStatus, TextFormat,
};
use slint::{Color, Model, ModelRc, StyledText, VecModel, Weak};
use tokio::{
    runtime::Handle,
    sync::{Mutex, mpsc},
    task::JoinHandle,
};

use crate::attachments::begin_download_save;
use crate::draft_recovery::{self, PrepareError, SendScope};
use crate::drafts::{
    DraftStore, PendingHandle, PendingItem, PendingStatus, PendingSummary, PendingTarget,
    Reconciled, Resolution, WriteTransaction,
};
use crate::message_search;
use crate::voice_spaces::VoiceSpaces;
use crate::{
    AccountItem, AccountOption, AppWindow, MemberItem, MessageItem, PublicRoomItem, RoomItem,
    SpaceItem,
};

/// Events loaded around a message opened in a focused conversation view.
const FOCUS_CONTEXT_EVENTS: u16 = 24;
/// Longest wait for the account transition before an invitation answer is
/// sent, and again before its result is recorded and opened: past it,
/// nothing is sent, or nothing opens.
const INVITE_TRANSITION_WAIT: Duration = Duration::from_secs(10);

pub enum MessageTarget {
    Room,
    Reply(String),
    Edit(String),
    Thread(String),
}

/// How a joined room opens, once sync shows it joined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JoinedNavigation {
    Room,
    Space,
}

/// What a confirmed join opens: nothing until sync shows the room joined
/// (`synced`: `Some(is_space)`), so no local state or navigation is ever
/// changed for a room that has not arrived.
pub(crate) fn joined_navigation(synced: Option<bool>) -> Option<JoinedNavigation> {
    synced.map(|is_space| {
        if is_space {
            JoinedNavigation::Space
        } else {
            JoinedNavigation::Room
        }
    })
}

/// What a join captures synchronously at the user's click, on the UI
/// thread: the navigation and security generations then, and its
/// withdrawal (a Close or Escape). Sampled later, in the worker, either
/// would admit a click made before a navigation or a security step.
#[derive(Clone)]
pub(crate) struct JoinAsked {
    navigation: u64,
    security: u64,
    cancellation: JoinCancellation,
    authority: NavigationAuthority,
    /// The join's own open, once its snapshot was shown (`owned_open`).
    opened: Arc<std::sync::Mutex<Option<JoinOpened>>>,
}

/// What a creation captures at its click (Create, Add to Space again, or
/// Open on a created room that is not open), on the UI thread: the
/// navigation and the security gate's generation then
/// (LiveUi::creation_asked). Its writes are handed off only while no
/// security or account step began since (creation_allowed,
/// creation_admits). The open later navigates only while nothing navigated
/// since that click and no security step began since (a step running at
/// the click never admits either), under the exact selection the room was
/// created for (open_created_room). Taken later, in the worker, either
/// would let a creation's completion move a person who has navigated,
/// switched or locked since.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CreationAsked {
    navigation: u64,
    security: u64,
}

/// What a join's own open showed: the navigation it took under the join's
/// authority (open_joined), and the exact room. A follow-up on that screen
/// runs only under this receipt (`JoinAsked::owned_open`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JoinOpened {
    pub(crate) navigation: u64,
    pub(crate) room_id: String,
}

impl JoinAsked {
    /// A join asked for now, with the navigation generation `navigation`.
    /// The gate's generation is read first: a step beginning after it
    /// shows either as blocked here or as a newer generation at the
    /// hand-off. A step running now refuses the join (u64::MAX never
    /// admits).
    fn at(gate: &crate::security::SecurityGate, navigation: u64) -> Self {
        let generation = gate.ringing_generation();
        let security = if gate.blocked() { u64::MAX } else { generation };
        Self {
            navigation,
            security,
            cancellation: JoinCancellation::new(),
            authority: NavigationAuthority::default(),
            opened: Arc::default(),
        }
    }

    /// Record the join's own open once its snapshot was shown
    /// (open_joined only).
    fn record_opened(&self, navigation: u64, room_id: String) {
        let opened = JoinOpened {
            navigation,
            room_id,
        };
        *self
            .opened
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(opened);
    }

    /// The join's own open, only while it still owns what is shown: its
    /// navigation is still the current one (nothing navigated since, a
    /// same-room activation or an account switch included) and no security
    /// step began since its click. The join's authority is no part of it:
    /// join_target_for withdraws that once it returns (JoinNavigationWait),
    /// and the requester's Close or replacement ends its own record first.
    /// A follow-up on that screen runs under this receipt's navigation; one
    /// taken later is never proof of it. Callers: matrix_links' finish_join
    /// only; every other join path ignores it.
    pub(crate) fn owned_open(&self, live: &LiveUi) -> Option<JoinOpened> {
        let opened = self
            .opened
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()?;
        let owned = live.navigation_is_current(opened.navigation)
            && live.security_gate.ringing_allowed(self.security);
        owned.then_some(opened)
    }

    /// The authority a follow-up of the join's own open runs under (a joined
    /// link's message): it holds only while the session is live, the join's
    /// exact `selection` is current and no security step began since the
    /// join's click (the generation the join itself was admitted under).
    /// Every synchronous commit and every admission of the follow-up asks
    /// it, its requester withdraws it (Close, a newer link, an account
    /// change, a protected surface), and it never takes, advances or blesses
    /// a navigation. Callers: matrix_links' finish_join only.
    pub(crate) fn follow_up_authority(
        &self,
        live: &LiveUi,
        selection: &AccountSelection,
    ) -> NavigationAuthority {
        let (is_live, gate) = (live.is_live.clone(), live.security_gate.clone());
        let (selection, security) = (selection.clone(), self.security);
        NavigationAuthority::holding(Arc::new(move || {
            is_live.load(Ordering::Acquire)
                && selection.is_current()
                && gate.ringing_allowed(security)
        }))
    }

    /// Withdraw the join: before its hand-off nothing is sent (true); after
    /// it the request keeps its own bounds and only opening the room is
    /// withdrawn (false). Its outcome stays in the join record either way.
    pub(crate) fn withdraw(&self) -> bool {
        self.authority.withdraw();
        self.cancellation.cancel()
    }

    #[cfg(test)]
    pub(crate) fn withdrawn_for_test(&self) -> bool {
        self.authority.withdrawn() && self.cancellation.is_cancelled()
    }

    /// What a caller's own bound reports when it ends the join: unknown
    /// once the request was handed off, nothing sent before.
    pub(crate) fn expired(&self) -> JoinError {
        if self.cancellation.sent() {
            JoinError::OutcomeUnknown
        } else {
            JoinError::Unavailable
        }
    }
}

/// Joins asked for in this run whose outcome a join UI may no longer show:
/// one still in flight, or one handed off with no definite reply. Kept per
/// account and typed room or address, whatever the Join a room dialog, the
/// directory or the Space explorer shows, hides or reopens since.
#[derive(Default)]
struct JoinOutcomes(std::sync::Mutex<JoinLedger>);

#[derive(Default)]
struct JoinLedger {
    next: u64,
    entries: HashMap<(String, String), (u64, JoinOutcome)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JoinOutcome {
    InFlight,
    Unknown,
}

/// Joins remembered at most. Unresolved entries are never evicted to make
/// another mutation possible; a full ledger pauses new joins instead.
const MAX_JOIN_OUTCOMES: usize = 64;

impl JoinOutcomes {
    fn outcomes(&self) -> std::sync::MutexGuard<'_, JoinLedger> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Start a join of `key`. Refused while another is in flight. An earlier
    /// unknown outcome pauses further attempts. Returning an error is not
    /// acknowledgment: its UI may have closed before that error is shown.
    fn begin(
        &self,
        key: &(String, String),
        current: impl FnOnce() -> bool,
    ) -> Result<u64, JoinError> {
        let mut outcomes = self.outcomes();
        // Removal revokes the selection before forgetting its entries. A
        // queued old caller cannot insert after that forget barrier.
        if !current() {
            return Err(JoinError::AccountChanged);
        }
        match outcomes.entries.get(key).map(|(_, outcome)| outcome) {
            Some(JoinOutcome::InFlight) => return Err(JoinError::Pending),
            Some(JoinOutcome::Unknown) => return Err(JoinError::EarlierUnknown),
            None => {}
        }
        if outcomes.entries.len() >= MAX_JOIN_OUTCOMES {
            return Err(JoinError::UnresolvedLimit);
        }
        let Some(receipt) = outcomes.next.checked_add(1) else {
            return Err(JoinError::UnresolvedLimit);
        };
        outcomes.next = receipt;
        outcomes
            .entries
            .insert(key.clone(), (receipt, JoinOutcome::InFlight));
        Ok(receipt)
    }

    /// The join of `key` ended: kept as unknown when it may have happened.
    fn settle(&self, key: &(String, String), receipt: u64, unknown: bool) {
        let mut outcomes = self.outcomes();
        // Actual account removal may have forgotten this receipt. A late
        // completion must neither recreate it nor settle a newer operation.
        if outcomes.entries.get(key).map(|(owner, _)| *owner) != Some(receipt) {
            return;
        }
        if unknown {
            outcomes
                .entries
                .insert(key.clone(), (receipt, JoinOutcome::Unknown));
        } else {
            outcomes.entries.remove(key);
        }
    }

    fn forget_account(&self, account: &str) {
        self.outcomes()
            .entries
            .retain(|(owner, _), _| owner != account);
    }

    #[cfg(test)]
    fn get(&self, key: &(String, String)) -> Option<JoinOutcome> {
        self.outcomes()
            .entries
            .get(key)
            .map(|(_, outcome)| *outcome)
    }
}

/// Settles a join's record however its future ends. Dropped by a caller's
/// bound before settling, it is unknown once its request was handed off,
/// and forgotten before (nothing was sent).
struct JoinRecord<'a> {
    outcomes: &'a JoinOutcomes,
    key: (String, String),
    receipt: u64,
    cancellation: JoinCancellation,
    settled: bool,
}

impl<'a> JoinRecord<'a> {
    fn new(
        outcomes: &'a JoinOutcomes,
        key: (String, String),
        receipt: u64,
        cancellation: JoinCancellation,
    ) -> Self {
        Self {
            outcomes,
            key,
            receipt,
            cancellation,
            settled: false,
        }
    }

    fn settle(mut self, unknown: bool) {
        self.settled = true;
        self.outcomes.settle(&self.key, self.receipt, unknown);
    }
}

impl Drop for JoinRecord<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.outcomes
                .settle(&self.key, self.receipt, self.cancellation.sent());
        }
    }
}

/// Waiting for an independently owned navigation never abandons its
/// rollback. A caller timeout/drop withdraws it; the task settles its own
/// admitted snapshot before releasing the barrier.
struct JoinNavigationWait(JoinAsked);

impl Drop for JoinNavigationWait {
    fn drop(&mut self) {
        self.0.withdraw();
    }
}

struct KnownSpace {
    name: String,
    recorded_at: Instant,
}

/// Whom a member load belongs to: the exact selection and navigation of
/// the snapshot that showed its room (after_snapshot: an owned reveal's, or
/// a plain refresh's when its account has a selection), or, for a forced
/// refresh (refresh_members_for), its account's selection and the navigation
/// captured at its call, before any await. Every production load has one;
/// a load that cannot capture one does nothing. The load claims the
/// member epoch only while both are still current, checked at the claim
/// itself, after every await, so a stale snapshot never installs an epoch
/// over a newer navigation's load or leaves that load waiting for nothing.
/// The same owner then fences every publication of the load
/// (MemberPublication).
#[derive(Clone)]
struct MemberOwner {
    selection: AccountSelection,
    navigation_epoch: u64,
}

struct MemberRequest {
    account_id: String,
    room_id: String,
    started: Instant,
    epoch: u64,
    /// The claiming load's owner. Production always sets it
    /// (refresh_members_owned); None only in owner-free unit fixtures.
    owner: Option<MemberOwner>,
}

impl MemberRequest {
    /// Whether a load for `account_id`'s `room_id` that is not forced may
    /// rely on this earlier one instead of fetching again (the 30 s
    /// coalescing): same account and room, its epoch still the latest (every
    /// navigation and every newer load advances it), under 30 s old, and its
    /// own publications still admitted, that is, its owner (if any) still
    /// current under `navigation_epoch`. A request whose owner is no longer
    /// current is never relied on, so it cannot suppress a current one.
    fn serves(
        &self,
        account_id: &str,
        room_id: &str,
        member_epoch: u64,
        navigation_epoch: u64,
    ) -> bool {
        self.account_id == account_id
            && self.room_id == room_id
            && self.epoch == member_epoch
            && self.started.elapsed() < Duration::from_secs(30)
            && self.owner.as_ref().is_none_or(|owner| {
                owner.selection.is_current() && owner.navigation_epoch == navigation_epoch
            })
    }
}

/// What a member load may still publish, captured at its claim: the epoch
/// it claimed, its owner (exact selection and navigation) when it has one,
/// and the account and room it is for, with the live state it is read
/// against. Every application of its loading flag, members or error goes
/// through `publish`, on the UI thread, at the moment it applies: a load
/// that is no longer the latest, whose owner no longer owns the screen, or
/// whose account and room are not the ones shown changes nothing, so a
/// newer load's members, loading flag and error stay as they are. Nothing
/// newer is ever recaptured to adopt stale work.
#[derive(Clone)]
struct MemberPublication {
    epoch: u64,
    account_id: String,
    room_id: String,
    owner: Option<MemberOwner>,
    is_live: Arc<AtomicBool>,
    member_epoch: Arc<AtomicU64>,
    navigation_epoch: Arc<AtomicU64>,
}

/// What a member load shows.
enum MemberUpdate {
    Loading,
    Members(Vec<MemberItem>),
    Error(String),
}

impl MemberPublication {
    /// Whether this load may still change what is shown for
    /// `visible_account` and `visible_room` (the window's own, in publish).
    fn admits(&self, visible_account: &str, visible_room: &str) -> bool {
        self.is_live.load(Ordering::Acquire)
            && self.member_epoch.load(Ordering::Acquire) == self.epoch
            && self.owner.as_ref().is_none_or(|owner| {
                owner.selection.is_current()
                    && self.navigation_epoch.load(Ordering::Acquire) == owner.navigation_epoch
            })
            && visible_account == self.account_id
            && visible_room == self.room_id
    }

    /// Apply `update` to `window` only while this load is admitted for what
    /// the window shows; whether it applied. UI thread only.
    fn publish(&self, window: &AppWindow, update: MemberUpdate) -> bool {
        let account = window.get_active_account_local_id();
        let room = window.get_active_room_id();
        if !self.admits(account.as_str(), room.as_str()) {
            return false;
        }
        match update {
            MemberUpdate::Loading => {
                window.set_member_loading(true);
                window.set_member_error("".into());
            }
            MemberUpdate::Members(items) => {
                window.set_member_loading(false);
                window.set_members(ModelRc::new(VecModel::from(items)));
                window.set_member_error("".into());
            }
            MemberUpdate::Error(error) => {
                window.set_member_loading(false);
                window.set_member_error(error.into());
            }
        }
        true
    }
}

/// Authority of one navigation its requester may withdraw, such as opening
/// a search result. Withdrawal and the navigation's commit take the same
/// lock, so once `withdraw` returns the navigation commits nothing more.
/// Withdrawing never touches the shared navigation epoch, so it cannot
/// cancel a newer navigation of anyone else.
///
/// An authority may also carry its requester's own captured conditions
/// (`holding`: for example a joined link's session, exact selection and
/// security gate as they were at its click). They are asked at every
/// synchronous commit, under the withdrawal lock (`commit_guard`), and
/// wherever `withdrawn` is asked, including a snapshot's or a focused view's
/// admission on the UI thread, so a security step that began (and maybe
/// ended) since the click stops the navigation there, with no navigation
/// change and no watcher needed.
///
/// It may also name the surfaces that end it (`blocked_by`, asked on the
/// UI thread by `admits`, an owned snapshot's admission included): a step
/// admitted while one of them is up withdraws the authority for good, so
/// that surface closing again never revives it.
///
/// Whether an owned snapshot was admitted and whether the authority was
/// withdrawn are one linearized decision, under the same lock, from any
/// thread: the UI thread claims the admission (`claim`, from
/// SnapshotGate::admits) just before it applies the snapshot, outside the
/// lock, and `withdraw` reports whether it came after such a claim. So a
/// requester that withdraws on its own bound never reads a claimed (shown or
/// being applied) snapshot as refused, and once withdrawn nothing more is
/// claimed or committed. No lock is held across the apply, which runs Slint
/// code that may withdraw this same authority. A withdrawal also wakes
/// every wait before a commit or an admission (`until_withdrawn`), so a
/// withdrawn reveal gives up its locks at once.
#[derive(Clone, Default)]
pub(crate) struct NavigationAuthority {
    shared: Arc<AuthorityState>,
    /// The requester's conditions besides its withdrawal; `None`: none.
    holds: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Whether another surface is up, asked on the UI thread; `None`: none.
    blocked_by: Option<fn(&AppWindow) -> bool>,
}

#[derive(Default)]
struct AuthorityState {
    decision: std::sync::Mutex<AuthorityDecision>,
    /// Woken by every withdrawal (until_withdrawn).
    wake: tokio::sync::Notify,
}

#[derive(Default)]
struct AuthorityDecision {
    withdrawn: bool,
    /// The UI thread claimed the admission of a snapshot under it (`claim`).
    admitted: bool,
}

impl NavigationAuthority {
    /// An authority that also needs `holds` at every commit and admission.
    /// Callers: JoinAsked::follow_up_authority (a joined link's message),
    /// OpenRequest::owned (an accepted invitation's own open, at its click)
    /// and LiveUi::creation_authority (a created room's open).
    pub(crate) fn holding(holds: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        Self {
            shared: Arc::default(),
            holds: Some(holds),
            blocked_by: None,
        }
    }

    /// The same authority, also ended for good by its first UI-thread
    /// admission while `blocked` says another surface is up. Set before it
    /// is cloned. Callers: matrix_links' finish_join (a joined link's
    /// message, blocked by matrix_links::surface_blocked) and
    /// LiveUi::creation_authority (a created room's open, blocked by
    /// room_creation::surface_blocked).
    pub(crate) fn blocked_by(mut self, blocked: fn(&AppWindow) -> bool) -> Self {
        self.blocked_by = Some(blocked);
        self
    }

    /// A UI-thread admission: not withdrawn, its own conditions holding and
    /// no surface it names up. A surface that is up withdraws it first.
    pub(crate) fn admits(&self, ui: &AppWindow) -> bool {
        if self.withdrawn() {
            return false;
        }
        if self.blocked_by.is_some_and(|blocked| blocked(ui)) {
            self.withdraw();
            return false;
        }
        true
    }

    /// Withdraw it, from any thread. Returns whether a snapshot under it was
    /// already claimed by the UI: that snapshot is then shown (or being
    /// applied right now), and only later commits and admissions are
    /// refused. Callers that do not need the answer ignore it.
    pub(crate) fn withdraw(&self) -> bool {
        let admitted = {
            let mut state = self.state();
            state.withdrawn = true;
            state.admitted
        };
        self.shared.wake.notify_waiters();
        admitted
    }

    /// Withdrawn, or its own conditions no longer hold.
    pub(crate) fn withdrawn(&self) -> bool {
        self.state().withdrawn || !self.holds()
    }

    /// Whether the UI claimed the admission of a snapshot under it.
    pub(crate) fn admitted(&self) -> bool {
        self.state().admitted
    }

    /// Claim the admission of a snapshot, on the UI thread just before it is
    /// applied: refused once withdrawn or once its own conditions fail, both
    /// read under the withdrawal lock.
    pub(crate) fn claim(&self) -> bool {
        let mut state = self.state();
        if state.withdrawn || !self.holds() {
            return false;
        }
        state.admitted = true;
        true
    }

    /// Resolves once withdrawn. A `holding` authority whose conditions
    /// already fail resolves at once; one whose conditions fail later wakes
    /// at its requester's withdrawal (every requester bounds its wait).
    pub(crate) async fn until_withdrawn(&self) {
        loop {
            let notified = self.shared.wake.notified();
            tokio::pin!(notified);
            // Register before checking: notify_waiters keeps no permit for a
            // later waiter.
            notified.as_mut().enable();
            if self.withdrawn() {
                return;
            }
            notified.await;
        }
    }

    /// Whether `other` is this very authority (a clone of it, sharing its
    /// one decision), not merely an equal one. Callers:
    /// matrix_links::release_automatic_open.
    pub(crate) fn is_same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    fn holds(&self) -> bool {
        self.holds.as_ref().is_none_or(|holds| holds())
    }

    /// Held only around synchronous commits and decisions.
    fn state(&self) -> std::sync::MutexGuard<'_, AuthorityDecision> {
        self.shared
            .decision
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// The withdrawal lock, held around one synchronous commit, only while
    /// the navigation may still commit: not withdrawn, and its own
    /// conditions holding when the commit takes it.
    fn commit_guard(&self) -> Option<std::sync::MutexGuard<'_, AuthorityDecision>> {
        let state = self.state();
        (!state.withdrawn && self.holds()).then_some(state)
    }
}

/// `work`, unless `authority` is withdrawn first (checked first on every
/// poll): `None` then, and `work` is dropped where it waited. Only for work
/// before a commit, or before an owned snapshot is decided, so that dropping
/// it leaves nothing shown and nothing but the commit to give back.
pub(crate) async fn unless_withdrawn<F: std::future::Future>(
    authority: &NavigationAuthority,
    work: F,
) -> Option<F::Output> {
    tokio::select! {
        biased;
        () = authority.until_withdrawn() => None,
        output = work => Some(output),
    }
}

/// `work` for a refresh, or for an owned reveal unless its authority is
/// withdrawn first (`None` then). Every wait before a reveal's snapshot is
/// admitted goes through it, so a withdrawn reveal never keeps its barrier
/// or the transition lock for a wait.
async fn within<F: std::future::Future>(
    stop: Option<&NavigationAuthority>,
    work: F,
) -> Option<F::Output> {
    match stop {
        Some(authority) => unless_withdrawn(authority, work).await,
        None => Some(work.await),
    }
}

/// What a shown snapshot leaves to do, scoped to exactly it: its account
/// and room, the selection its feed opens under (a reveal's own, never one
/// looked up again), the navigation epoch it was built under, and for an
/// owned reveal the left room's thread generation and its draft error.
struct AfterSnapshot {
    account_id: String,
    room_id: Option<String>,
    selection: Option<AccountSelection>,
    navigation_epoch: u64,
    marker_rooms: Vec<String>,
    owned: Option<(u64, Option<String>)>,
    /// An owned joined Space shown with no room: the left room's feed is
    /// retired with it, while that snapshot is still the current one.
    retire_feed: bool,
    /// Recover the room's drafts (a room reveal), while it is still shown.
    recover: bool,
}

/// What an automatic open was chosen under, captured on the UI thread at
/// its click (`LiveUi::open_request`): the navigation then, and its own
/// conditions, which the open's authority asks at its commit and at its
/// snapshot's admission (`NavigationAuthority::holding`).
#[derive(Clone)]
pub(crate) struct OpenRequest {
    navigation: u64,
    holds: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl OpenRequest {
    /// The open owned from its click by `selection`, the click's exact
    /// account: the navigation then and the open's one authority, made now
    /// from the click's conditions and that selection. Its holder hands that
    /// authority to matrix_links::own_automatic_open at the click, so every
    /// protected surface's entry withdraws it for good; its commit and its
    /// snapshot's admission ask it (open_answered).
    pub(crate) fn owned(self, selection: &AccountSelection) -> OwnedOpen {
        let (account, holds) = (selection.clone(), self.holds);
        let owned = move || account.is_current() && holds();
        OwnedOpen {
            navigation: self.navigation,
            authority: NavigationAuthority::holding(Arc::new(owned)),
        }
    }
}

/// An automatic open owned from its click (`OpenRequest::owned`).
#[derive(Clone)]
pub(crate) struct OwnedOpen {
    navigation: u64,
    authority: NavigationAuthority,
}

impl OwnedOpen {
    /// The open's one authority, from its click to its commit.
    pub(crate) fn authority(&self) -> &NavigationAuthority {
        &self.authority
    }
}

/// What a reveal's snapshot must still hold when it is applied: the
/// navigation's withdrawable authority, the account selection and the
/// navigation epoch that started it. Decided on the UI thread, where the
/// admission is claimed on the authority (see NavigationAuthority).
#[derive(Clone)]
struct SnapshotGate {
    authority: NavigationAuthority,
    selection: AccountSelection,
    navigation: u64,
}

impl SnapshotGate {
    /// Decide the snapshot on the UI thread, just before it is applied: the
    /// selection and epoch still current, then the admission claimed on the
    /// authority, last, so a claim is recorded only for a snapshot that is
    /// then applied. A withdrawn authority (or one whose own conditions
    /// fail) never admits, as before.
    fn admits(&self, navigation: &AtomicU64) -> bool {
        self.selection.is_current()
            && navigation.load(Ordering::Acquire) == self.navigation
            && self.authority.claim()
    }

    /// `admits`, first asking the authority on the window: a surface it
    /// names that is up now withdraws it for good (NavigationAuthority::
    /// admits), so the snapshot is refused and the commit is given back.
    /// Only an authority with `blocked_by` can be refused here; for every
    /// other one this is exactly `admits`.
    fn admits_on(&self, ui: &AppWindow, navigation: &AtomicU64) -> bool {
        self.authority.admits(ui) && self.admits(navigation)
    }
}

/// A committed reveal whose snapshot is not yet decided. It holds the
/// snapshot barrier from before it reads the rooms, so no other refresh
/// samples the committed room (and could show it unguarded) until the
/// reveal's own snapshot was applied or the reveal gave the room back.
struct RevealOwner<'a> {
    gate: SnapshotGate,
    /// The room the reveal committed; its snapshot shows exactly this room.
    room_id: String,
    /// A joined Space owns the Space snapshot instead of one exact room.
    /// Its selected room is chosen by the ordinary visibility rules.
    space_id: Option<String>,
    /// Held for its guard; released once the snapshot was shown.
    _barrier: Option<tokio::sync::RwLockWriteGuard<'a, ()>>,
    /// Told the moment the snapshot is admitted, before any follow-up
    /// (members, the feed, recovery). Dropped unsent when it never is.
    /// Only a created room's open asks for it (open_created_room).
    admitted: Option<tokio::sync::oneshot::Sender<()>>,
}

/// Actual draft/send task ownership for this application session. Terminal
/// admission and task registration share one mutex; no task can appear after
/// close/drain. Completion is the retained JoinHandle's actual poll receipt,
/// never a delivered UI result, lease/count snapshot or `is_finished` hint.
#[derive(Default)]
struct DraftWork {
    closed: bool,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    failed: bool,
}
impl DraftWork {
    fn spawn(
        &mut self,
        runtime: &Handle,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> bool {
        let _ = self.poll();
        if self.closed || self.failed {
            return false;
        }
        self.tasks.push(runtime.spawn(task));
        true
    }

    fn poll(&mut self) -> Option<Result<(), DraftDrainError>> {
        use std::future::Future;
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let mut index = 0;
        while index < self.tasks.len() {
            match std::pin::Pin::new(&mut self.tasks[index]).poll(&mut context) {
                std::task::Poll::Ready(result) => {
                    self.failed |= result.is_err();
                    drop(self.tasks.swap_remove(index));
                }
                std::task::Poll::Pending => index += 1,
            }
        }
        self.tasks.is_empty().then_some(if self.failed {
            Err(DraftDrainError)
        } else {
            Ok(())
        })
    }
}
#[cfg(test)]
mod draft_work_shutdown_tests {
    use super::*;
    #[test]
    fn terminal_task_close_keeps_the_actual_entered_task_until_join_receipt() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut work = DraftWork::default();
            let (release, released) = tokio::sync::oneshot::channel();
            let (started, beginning) = tokio::sync::oneshot::channel();
            let admitted = work.spawn(runtime.handle(), async move {
                let _ = started.send(());
                let _ = released.await;
            });
            let entered = tokio::time::timeout(Duration::from_secs(3), beginning).await;
            work.closed = true;
            let polled = work.poll();
            let refused = !work.spawn(runtime.handle(), async {});
            let _ = release.send(());
            let cleanup = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Some(result) = work.poll() {
                        return result;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await;
            if cleanup.is_err() {
                std::mem::forget(work);
                panic!("Actual draft task cleanup remained unproved");
            }
            assert!(admitted);
            assert!(matches!(entered, Ok(Ok(()))));
            assert!(polled.is_none());
            assert!(refused);
            assert!(matches!(cleanup, Ok(Ok(()))));
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DraftDrainError;
impl std::fmt::Display for DraftDrainError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("Draft/send task completion could not be confirmed.")
    }
}
impl std::error::Error for DraftDrainError {}

pub struct LiveUi {
    pub manager: Arc<AccountManager>,
    pub(crate) voice_spaces: Arc<VoiceSpaces>,
    pub is_live: Arc<AtomicBool>,
    pub(crate) security_gate: Arc<crate::security::SecurityGate>,
    /// Joins whose outcome no UI may still show (see JoinOutcomes).
    join_outcomes: JoinOutcomes,
    drafts: Arc<DraftStore>,
    draft_write_tx: std::sync::Mutex<Option<mpsc::Sender<()>>>,
    draft_work: std::sync::Mutex<DraftWork>,
    verification_work: crate::verification_work::VerificationWork,
    marker_scans: crate::marker_scans::MarkerScans,
    generation: Arc<AtomicU64>,
    recovery_notices: Arc<crate::timeline_recovery::Notices>,
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
    /// The one focused conversation view shown, owned here (see FocusSlot).
    focused: std::sync::Mutex<FocusSlot<morpheus_matrix::FocusedView>>,
    /// Taken for reading by every refresh until its snapshot is queued, and
    /// for writing by a reveal from before it reads the rooms until its
    /// snapshot is decided (see RevealOwner).
    snapshot_barrier: tokio::sync::RwLock<()>,
    #[cfg(test)]
    pub(crate) pauses: TestPauses,
}

/// The one focused conversation view the app shows, with the feed
/// generation that installed it. Loading a view never touches the slot; only
/// a commit that still holds its navigation authority installs one, and any
/// newer feed generation (a live feed, another view) makes it stale. Paging
/// takes exactly the view shown under the current generation.
struct FocusSlot<V> {
    shown: Option<(u64, V)>,
}

impl<V> Default for FocusSlot<V> {
    fn default() -> Self {
        Self { shown: None }
    }
}

impl<V: Clone> FocusSlot<V> {
    /// Install `view` under a new feed generation, only while `authority`
    /// holds (with its own conditions). Withdrawal takes the same lock, so a
    /// superseded view whose open was withdrawn is never installed. Returns
    /// that generation.
    fn commit(
        &mut self,
        generation: &AtomicU64,
        authority: &NavigationAuthority,
        view: V,
    ) -> Option<u64> {
        let _admitted = authority.commit_guard()?;
        let installed = generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.shown = Some((installed, view));
        Some(installed)
    }

    /// The view shown under feed generation `current`, if any.
    fn view(&self, current: u64) -> Option<V> {
        match &self.shown {
            Some((installed, view)) if *installed == current => Some(view.clone()),
            _ => None,
        }
    }

    /// Forget a view that a newer feed generation replaced.
    fn drop_stale(&mut self, current: u64) {
        if self
            .shown
            .as_ref()
            .is_some_and(|(installed, _)| *installed != current)
        {
            self.shown = None;
        }
    }
}

/// A committed focused view whose open was withdrawn before any of its rows
/// were shown (see focus_event_for). Giving it back ends it and reopens the
/// room's live feed, which the commit ended; nothing happens if another view
/// or feed has replaced it since.
#[derive(Clone)]
struct UnshownView {
    live: std::sync::Weak<LiveUi>,
    runtime: Handle,
    view: morpheus_matrix::FocusedView,
    ui: Weak<AppWindow>,
}

impl UnshownView {
    fn give_back(self, selection: AccountSelection, room_id: String) {
        let Some(live) = self.live.upgrade() else {
            return;
        };
        let (view, ui) = (Some(self.view), self.ui);
        self.runtime.spawn(async move {
            live.return_to_live_for(selection, room_id, view, ui).await;
        });
    }
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

    pub(crate) async fn reach(&self, name: &'static str) {
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
        let recovery_notices = crate::timeline_recovery::Notices::new(ui.clone());
        let (draft_write_tx, mut draft_write_rx) = mpsc::channel(1);
        let drafts = Arc::new(DraftStore::default());
        let draft_worker_store = drafts.clone();
        let draft_worker_manager = manager.clone();
        let draft_writer = runtime.spawn(async move {
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
            join_outcomes: JoinOutcomes::default(),
            drafts,
            draft_write_tx: std::sync::Mutex::new(Some(draft_write_tx)),
            draft_work: std::sync::Mutex::new(DraftWork {
                tasks: vec![draft_writer],
                ..DraftWork::default()
            }),
            verification_work: crate::verification_work::VerificationWork::default(),
            marker_scans: crate::marker_scans::MarkerScans::default(),
            generation: Arc::new(AtomicU64::new(0)),
            recovery_notices,
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
            focused: std::sync::Mutex::new(FocusSlot::default()),
            snapshot_barrier: tokio::sync::RwLock::new(()),
            #[cfg(test)]
            pauses: TestPauses::default(),
        })
    }

    /// Permanent application action boundary, called synchronously on the UI
    /// thread before it yields. Loaded stores stay owned for the admitted work
    /// drain and final stable flush; resumable manager.stop remains distinct.
    pub(crate) fn close_action_admission(&self) {
        self.verification_work.close();
        self.marker_scans.close();
        let mut work = self
            .draft_work
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        work.closed = true;
        self.manager.close_selection_admission();
        // This is the only sender. The existing writer finishes an entered
        // write, observes EOF, processes its last dirty sweep and exits.
        self.draft_write_tx
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }

    /// Capture synchronously with a verification UI intent. Reusing a local
    /// account ID after a removal pause never revives an older lease.
    pub(crate) fn verification_work_lease(
        &self,
        selection: &AccountSelection,
        cancellation: &morpheus_matrix::VerificationCancellation,
    ) -> Option<crate::verification_work::VerificationWorkLease> {
        self.verification_work.lease(selection, cancellation)
    }

    pub(crate) fn spawn_verification_work(
        &self,
        runtime: &Handle,
        lease: &crate::verification_work::VerificationWorkLease,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> bool {
        self.verification_work.spawn(runtime, lease, task)
    }

    pub(crate) fn pause_verification_work(
        &self,
        account: &str,
    ) -> Result<
        crate::verification_work::VerificationWorkPause,
        crate::verification_work::VerificationDrainError,
    > {
        self.verification_work.pause(account)
    }

    pub(crate) async fn drain_verification_work(
        &self,
    ) -> Result<(), crate::verification_work::VerificationDrainError> {
        self.verification_work.drain().await
    }

    pub(crate) fn action_admission_closed(&self) -> bool {
        self.draft_work
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .closed
    }

    pub(crate) fn spawn_draft_work(
        &self,
        runtime: &Handle,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> bool {
        let mut work = self
            .draft_work
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        work.spawn(runtime, task)
    }

    /// No timeout drops/takes the sole task handles. The UI keeps pumping;
    /// an entered store operation must actually return before stable flush.
    pub(crate) async fn drain_draft_work(&self) -> Result<(), DraftDrainError> {
        loop {
            let result = {
                let mut work = self
                    .draft_work
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if !work.closed {
                    return Err(DraftDrainError);
                }
                work.poll()
            };
            if let Some(result) = result {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Cancel queued presentation work as soon as the user requests a new
    /// account, room, or Space, before the async operation starts running.
    pub fn invalidate_navigation(&self) -> u64 {
        self.render_epoch.fetch_add(1, Ordering::AcqRel);
        self.member_epoch.fetch_add(1, Ordering::AcqRel);
        let navigation = self.navigation_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        self.recovery_notices.clear_all();
        navigation
    }

    pub(crate) fn attachment_audio_lease(
        &self,
        thread: bool,
    ) -> crate::audio_attachments::ViewLease {
        let navigation_at = self.navigation_epoch.load(Ordering::Acquire);
        let security_at = self.security_gate.ringing_generation();
        let generation = if thread {
            self.thread_generation.clone()
        } else {
            self.generation.clone()
        };
        let generation_at = generation.load(Ordering::Acquire);
        crate::audio_attachments::ViewLease {
            live: self.is_live.clone(),
            navigation: self.navigation_epoch.clone(),
            navigation_at,
            generation,
            generation_at,
            gate: self.security_gate.clone(),
            security_at,
        }
    }

    pub(crate) fn current_navigation_epoch(&self) -> u64 {
        self.navigation_epoch.load(Ordering::Acquire)
    }

    /// Capture a join at the user's click; call it on the UI thread, in the
    /// click's callback, before anything is spawned. The security gate's
    /// generation advances with every reservation (a security step, account
    /// access, a restart), so a step that began after the click is refused
    /// at the hand-off even if it has already ended; one running at the
    /// click is refused too.
    pub(crate) fn join_asked(&self) -> JoinAsked {
        JoinAsked::at(&self.security_gate, self.current_navigation_epoch())
    }

    /// Capture an automatic open at the click, on the UI thread: the
    /// navigation now, and its own conditions, the session live and the
    /// security gate still at `security` (the generation the answer was
    /// chosen under: no step in progress or begun since). Callers: the
    /// invitations Home card's accept (InvitationsCenter::answer).
    pub(crate) fn open_request(&self, security: u64) -> OpenRequest {
        let (gate, is_live) = (self.security_gate.clone(), self.is_live.clone());
        OpenRequest {
            navigation: self.current_navigation_epoch(),
            holds: Arc::new(move || {
                is_live.load(Ordering::Acquire) && gate.ringing_allowed(security)
            }),
        }
    }

    /// Take a new navigation for an automatic open, only while `epoch` (the
    /// one it was chosen in) is still the current one, in one step: a newer
    /// navigation is never invalidated, and nothing changes when it fails.
    /// Callers: open_answered.
    pub(crate) fn claim_navigation_from(&self, epoch: u64) -> Option<u64> {
        if !self.is_live.load(Ordering::Acquire) {
            return None;
        }
        let next = epoch.checked_add(1)?;
        self.navigation_epoch
            .compare_exchange(epoch, next, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        self.recovery_notices.clear_all();
        self.render_epoch.fetch_add(1, Ordering::AcqRel);
        self.member_epoch.fetch_add(1, Ordering::AcqRel);
        Some(next)
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
        let work = self
            .draft_work
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if work.closed {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
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
        let work = self
            .draft_work
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if work.closed {
            return;
        }
        if !account_id.is_empty() && !room_id.is_empty() {
            self.drafts.set(account_id, room_id, body);
            self.queue_draft_write();
        }
    }

    pub fn remember_markdown(&self, account_id: &str, room_id: &str, enabled: bool) {
        let work = self
            .draft_work
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if work.closed {
            return;
        }
        if !account_id.is_empty() && !room_id.is_empty() {
            self.drafts.set_markdown(account_id, room_id, enabled);
        }
    }

    pub fn remember_thread_draft(&self, account_id: &str, room_id: &str, root: &str, body: &str) {
        let work = self
            .draft_work
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if work.closed {
            return;
        }
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
        if self.action_admission_closed() {
            return None;
        }
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
            {
                let work = self
                    .draft_work
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if work.closed {
                    return;
                }
                self.drafts.seed_recovered(
                    &scope.account,
                    &scope.room,
                    scope.thread(),
                    draft.restored,
                    &attached,
                    epoch,
                );
            }
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

    /// Clear private draft buffers even when local removal is incomplete.
    pub fn forget_account_drafts(&self, account_id: &str) {
        self.drafts.forget_account(account_id);
    }

    /// Metadata only. Call after complete local removal or successful
    /// logout, never for a pause, account switch or incomplete removal.
    pub(crate) fn forget_removed_account_joins(&self, account_id: &str) {
        self.join_outcomes.forget_account(account_id);
    }

    pub async fn flush_drafts(&self) -> Result<(), MatrixCoreError> {
        self.drafts.flush_dirty(&self.manager).await
    }

    fn queue_draft_write(&self) {
        // One queued signal is enough: the worker scans the latest dirty
        // cache, coalescing fast typing without creating a task per keypress.
        if let Some(sender) = self
            .draft_write_tx
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            let _ = sender.try_send(());
        }
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
        // What the Space panel learned never outlives the shown account.
        crate::space_management::forget_all();
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
            crate::kept_replies::forget_all();
            crate::reset_attachment_view(&ui);
            clear_account_context(&ui, &gate);
            // Locked or detached: no account picture stays in memory or view.
            crate::account_identity::forget_all_avatars(&ui);
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
            crate::room_organization::clear(&ui);
            ui.set_messages(ModelRc::new(VecModel::<MessageItem>::default()));
            ui.set_conversation_focused(false);
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
            crate::withdraw_call_bar(&ui);
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

    /// Whether a creation asked at `asked` may go on to its create request:
    /// with account_transition held (its caller bounds this wait on its own,
    /// so passing it sends nothing), the exact selection is the active,
    /// current account, and no security or account step began since the
    /// click or runs now. Nothing is sent here.
    pub(crate) async fn creation_allowed(
        &self,
        selection: &AccountSelection,
        asked: CreationAsked,
    ) -> Result<(), CreationError> {
        let _transition = self.account_transition.lock().await;
        let active = self.require_active_account(selection.account_id()).await;
        if !selection.is_current() || active.is_err() {
            return Err(CreationError::AccountChanged);
        }
        if !self.security_gate.ringing_allowed(asked.security) {
            return Err(CreationError::SecurityBusy);
        }
        Ok(())
    }

    /// The admission of a creation's writes (its create request and every
    /// Space link of it), asked by the core with the exact selection right
    /// before each hand-off: the session is live and no security or account
    /// step began since the click (`asked`), the gate as it was then, as
    /// join_target_for's `admit` is.
    pub(crate) fn creation_admits(
        &self,
        asked: CreationAsked,
    ) -> impl Fn() -> bool + Send + Sync + 'static {
        let (gate, is_live) = (self.security_gate.clone(), self.is_live.clone());
        move || is_live.load(Ordering::Acquire) && gate.ringing_allowed(asked.security)
    }

    /// Create what the room-actions dialog planned, under the exact
    /// selection it was opened with, once creation_allowed said it may go:
    /// the create request (AccountManager::create_room_for), handed off only
    /// under `asked`'s admission, and nothing else. Not under
    /// account_transition: a slow homeserver must not hold up an account
    /// switch, and the exact selection fences every step. Placing, syncing
    /// and opening the room are later steps that cannot change this result.
    pub(crate) async fn create_room_for(
        &self,
        selection: &AccountSelection,
        plan: &CreationPlan,
        asked: CreationAsked,
    ) -> Result<CreatedRoom, CreationError> {
        let admit = self.creation_admits(asked);
        self.manager.create_room_for(selection, plan, admit).await
    }

    /// The navigation and security generations at a creation's click
    /// (Create, Add to Space again, or Open on a created room that is not
    /// open), read on the UI thread (see CreationAsked). The gate's generation is read first, as
    /// JoinAsked::at does.
    pub(crate) fn creation_asked(&self) -> CreationAsked {
        let generation = self.security_gate.ringing_generation();
        let security = if self.security_gate.blocked() {
            u64::MAX
        } else {
            generation
        };
        CreationAsked {
            navigation: self.current_navigation_epoch(),
            security,
        }
    }

    /// The authority a created room's open runs under: it holds only while
    /// the session is live, the creation's exact `selection` is current and
    /// no security step began since the click (`asked`), asked at every
    /// commit and admission (NavigationAuthority::holding, the shape of
    /// JoinAsked::follow_up_authority). It is also ended for good by its
    /// first UI-thread admission while a newer protected surface is up
    /// (blocked_by room_creation::surface_blocked: everything Leave counts
    /// but the page and the room-actions dialog it runs in, plus Leave, a
    /// link card and the header's More menu), so that surface closing never
    /// revives it. Built at the click on the UI thread and held there as the
    /// window's owned automatic open (room_creation's install_open), which
    /// every protected entry withdraws first (matrix_links::clear), and
    /// withdrawn by its bound (room_creation::bounded_reveal). It never
    /// takes, advances or blesses a navigation by itself.
    pub(crate) fn creation_authority(
        &self,
        asked: CreationAsked,
        selection: &AccountSelection,
    ) -> NavigationAuthority {
        let (is_live, gate) = (self.is_live.clone(), self.security_gate.clone());
        let (selection, security) = (selection.clone(), asked.security);
        NavigationAuthority::holding(Arc::new(move || {
            is_live.load(Ordering::Acquire)
                && selection.is_current()
                && gate.ringing_allowed(security)
        }))
        .blocked_by(crate::room_creation::surface_blocked)
    }

    /// Open a confirmed created room or Space as an owned reveal, the way a
    /// search result or a joined room opens (reveal_room_locked;
    /// reveal_joined_space_locked for a Space), which keeps every room's
    /// draft. A room opens inside the Space that lists it (`placement`),
    /// else in Home.
    ///
    /// It navigates only for its own click (`asked`): with the transition
    /// lock held and under `authority`'s withdrawal lock, it takes a new
    /// navigation epoch only while nothing navigated since that click, the
    /// exact `selection` is current and the authority holds
    /// (created_navigation). Otherwise nothing changes and the room is
    /// reported as not opened. From there:
    /// - before the commit, nothing changes;
    /// - the commit sets the exact target, and the feed is left running;
    /// - the snapshot that shows it is admitted only on the UI thread, which
    ///   claims the admission on `authority` while the epoch and selection
    ///   are still current, and that decision is awaited;
    /// - not admitted (withdrawn before a claim, replaced by a newer
    ///   navigation, another account, a security step since the click, not
    ///   visible there, its refresh failed, or no window): the committed
    ///   Space and room are given back before any other refresh samples
    ///   them, and reopen_unrevealed reopens the shown room's feed under the
    ///   epoch this open took, while it is still current.
    ///
    /// `admitted` is told the moment that snapshot is admitted showing
    /// exactly the created room, or the created Space as the selected one,
    /// before the follow-ups after it; it is dropped unsent otherwise, before
    /// any reopen. The result also says it (Ok(true)), but only after those
    /// follow-ups. Every wait before the admission gives way to a withdrawal
    /// (the transition lock, the barrier, every read and lock of the
    /// snapshot, the UI decision unless the UI already claimed it). The
    /// guarded part (navigation, commit, admission, give-back) holds the
    /// transition lock; the follow-ups (after_snapshot, each scoped to this
    /// snapshot) and the reopen of an unshown open run after releasing it.
    /// It never creates or links anything: the room exists whatever it
    /// returns.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open_created_room(
        self: Arc<Self>,
        selection: AccountSelection,
        created: CreatedRoom,
        placement: Placement,
        asked: CreationAsked,
        authority: NavigationAuthority,
        admitted: tokio::sync::oneshot::Sender<()>,
        ui: Weak<AppWindow>,
    ) -> Result<bool, MatrixCoreError> {
        let mut admitted = Some(admitted);
        // The transition guard lives only in this block: whatever branch is
        // taken, no residual guard reaches the follow-ups or the reopen.
        let (epoch, revealed) = {
            let transition = unless_withdrawn(&authority, self.account_transition.lock()).await;
            #[cfg(test)]
            self.pauses.reach("creation.after_transition").await;
            match transition {
                Some(_transition) => {
                    match self.created_navigation(&selection, asked, &authority) {
                        Some(epoch) => {
                            let told = admitted.take();
                            let shown = self
                                .reveal_created_locked(
                                    &selection, &created, &placement, epoch, &authority, told, &ui,
                                )
                                .await;
                            (Some(epoch), shown)
                        }
                        // Navigated since the click, another account,
                        // withdrawn or a security step since: nothing
                        // navigates.
                        None => (None, Ok(None)),
                    }
                }
                // Withdrawn while it waited for the transition lock.
                None => (None, Ok(None)),
            }
        };
        // Never admitted on these paths: the bound learns it now.
        drop(admitted);
        // The transition lock is released: the follow-ups (members, the feed,
        // recovery) and the reopen of an unshown open never hold up
        // navigation.
        let opened = match (revealed, epoch) {
            (Ok(Some(after)), _) => {
                self.after_snapshot(after, ui.clone()).await;
                Ok(true)
            }
            // Not admitted after this open took its epoch: that epoch retired
            // the shown room's feed, which is reopened under it while it is
            // still current (see reopen_unrevealed).
            (other, Some(epoch)) => {
                self.reopen_unrevealed(&selection, epoch, ui.clone()).await;
                other.map(|_| false)
            }
            (other, None) => other.map(|_| false),
        };
        // The Space graph is read again with the created room in it.
        tokio::spawn(self.clone().scan_spaces(ui));
        opened
    }

    /// The navigation of a created room's open, taken with the transition
    /// lock held and under `authority`'s withdrawal lock (`commit_guard`),
    /// only while it holds (not withdrawn; live, the exact selection current
    /// and no security step since the click), the selection is current and
    /// nothing navigated since the click (`asked`). `None` otherwise, and
    /// nothing navigates. The shape of open_joined's own navigation.
    fn created_navigation(
        &self,
        selection: &AccountSelection,
        asked: CreationAsked,
        authority: &NavigationAuthority,
    ) -> Option<u64> {
        let _admitted = authority.commit_guard()?;
        if !selection.is_current() || !self.navigation_is_current(asked.navigation) {
            return None;
        }
        Some(self.invalidate_navigation())
    }

    /// The guarded part of a created room's open, under the transition lock:
    /// a Space through reveal_joined_space_locked, a room through
    /// reveal_room_locked, after recording a confirmed Space link in the
    /// cached graph (a scan under way could install an older graph: it is
    /// invalidated first).
    #[allow(clippy::too_many_arguments)]
    async fn reveal_created_locked(
        self: &Arc<Self>,
        selection: &AccountSelection,
        created: &CreatedRoom,
        placement: &Placement,
        epoch: u64,
        authority: &NavigationAuthority,
        admitted: Option<tokio::sync::oneshot::Sender<()>>,
        ui: &Weak<AppWindow>,
    ) -> Result<Option<AfterSnapshot>, MatrixCoreError> {
        match created.kind {
            CreationKind::Space => {
                let space = created.room_id.clone();
                self.reveal_joined_space_locked(selection, space, epoch, authority, admitted, ui)
                    .await
            }
            CreationKind::Room => {
                let linked = async {
                    if let Placement::Linked { space_id } = placement {
                        self.invalidate_space_scan().await;
                        self.space_children
                            .lock()
                            .await
                            .entry(space_id.clone())
                            .or_default()
                            .insert(created.room_id.clone());
                    }
                };
                if unless_withdrawn(authority, linked).await.is_none() {
                    return Ok(None);
                }
                let room = created.room_id.clone();
                self.reveal_room_locked(selection, room, epoch, authority, admitted, ui)
                    .await
            }
        }
    }

    /// Join a parsed room ID, alias, or Matrix link (the directory, Join a
    /// room, the Space explorer) for the exact `selection` captured when it
    /// was asked for (AccountManager::join_room_for): one bounded request,
    /// refused while a security step runs or if one ran since the click
    /// (`asked`), never sent again. A selection is never current again once
    /// its account was left, so a join asked for before an A-B-A switch is
    /// not sent after it. One join per account and typed room at a time; an
    /// unknown outcome is kept (JoinOutcomes) whatever the UI does since.
    /// A withdrawal (`asked`) before the hand-off sends nothing; after it,
    /// nothing is opened.
    ///
    /// The account transition lock is not held across the request: a slow
    /// homeserver never holds up an account switch, and the selection
    /// fences every step. Nothing local changes until sync shows the room
    /// joined (see joined_navigation); then it opens through the guarded
    /// navigation as its own task, so no caller's bound can drop it between
    /// a commit and its give-back. The requester retains withdrawal until
    /// its actual snapshot finishes. Ok(true) when it was shown, Ok(false)
    /// when sync has not arrived or opening was withdrawn/superseded.
    pub(crate) async fn join_target_for(
        self: &Arc<Self>,
        selection: &AccountSelection,
        target: &MatrixLinkTarget,
        asked: &JoinAsked,
        ui: Weak<AppWindow>,
    ) -> Result<bool, MatrixCoreError> {
        if !self.is_live.load(Ordering::Acquire) || !selection.is_current() {
            return Err(JoinError::AccountChanged.into());
        }
        let MatrixLinkTarget::Room {
            room_or_alias_id, ..
        } = target
        else {
            return Err(JoinError::InvalidTarget.into());
        };
        let key = (selection.account_id().to_owned(), room_or_alias_id.clone());
        let receipt = self.join_outcomes.begin(&key, || selection.is_current())?;
        let record = JoinRecord::new(
            &self.join_outcomes,
            key,
            receipt,
            asked.cancellation.clone(),
        );
        // The gate as it was at the click, not as it is now.
        let (gate, is_live, security) = (
            self.security_gate.clone(),
            self.is_live.clone(),
            asked.security,
        );
        let admit = move || is_live.load(Ordering::Acquire) && gate.ringing_allowed(security);
        let joined = self
            .manager
            .join_room_for(selection, target, &asked.cancellation, admit)
            .await;
        record.settle(joined.as_ref().is_err_and(|error| error.may_have_joined()));
        let room_id = joined?;
        let synced = self
            .manager
            .await_joined_for(selection, &room_id, &asked.cancellation)
            .await;
        let Some(open) = joined_navigation(synced) else {
            return Ok(false);
        };
        // Keep the actual UI's withdrawal slot until navigation has finished
        // its admitted snapshot, rather than just until this task is spawned.
        let _waiting = JoinNavigationWait(asked.clone());
        let (finished, completion) = tokio::sync::oneshot::channel();
        let (live, selection, asked) = (self.clone(), selection.clone(), asked.clone());
        tokio::spawn(async move {
            let result = live.open_joined(selection, room_id, open, asked, ui).await;
            let _ = finished.send(result);
        });
        // The join itself was confirmed above. An unavailable/withdrawn
        // open is not a failed join and must not be reported as one.
        Ok(matches!(completion.await, Ok(Ok(true))))
    }

    /// Open a room or Space that sync shows joined, only while its selection
    /// is current, the join was not withdrawn and nothing navigated since
    /// the click (`asked`): a room as an owned reveal (reveal_room_for:
    /// committed exactly under the join's own authority, which a withdrawal
    /// revokes, shown only through its own admitted snapshot, given back
    /// otherwise), a joined Space through the same owned snapshot barrier.
    async fn open_joined(
        self: Arc<Self>,
        selection: AccountSelection,
        room_id: String,
        open: JoinedNavigation,
        asked: JoinAsked,
        ui: Weak<AppWindow>,
    ) -> Result<bool, MatrixCoreError> {
        // Close can end the wait for the transition immediately, without
        // dropping an already committed reveal or its give-back work.
        let _transition = tokio::select! {
            biased;
            _ = asked.cancellation.cancelled() => return Ok(false),
            transition = selection.guard(self.account_transition.lock()) => {
                let Some(transition) = transition else { return Ok(false); };
                transition
            }
        };
        // Do not invalidate the current feed while waiting for a held
        // transition: Close or a newer navigation may already have won.
        let epoch = {
            let state = asked.authority.state();
            if state.withdrawn
                || asked.cancellation.is_cancelled()
                || !selection.is_current()
                || !self.navigation_is_current(asked.navigation)
            {
                return Ok(false);
            }
            self.invalidate_navigation()
        };
        let shown_room = room_id.clone();
        let authority = &asked.authority;
        let opened = match open {
            JoinedNavigation::Room => {
                self.reveal_room_locked(&selection, room_id, epoch, authority, None, &ui)
                    .await
            }
            JoinedNavigation::Space => {
                self.reveal_joined_space_locked(&selection, room_id, epoch, authority, None, &ui)
                    .await
            }
        };
        match opened {
            // Its follow-ups still run under this open's transition lock, as
            // before; each is scoped to its own snapshot (after_snapshot).
            Ok(Some(after)) => {
                self.after_snapshot(after, ui).await;
                // Its receipt: the navigation this open took and the exact
                // room.
                asked.record_opened(epoch, shown_room);
                Ok(true)
            }
            other => {
                self.reopen_unrevealed(&selection, epoch, ui).await;
                other.map(|_| false)
            }
        }
    }

    /// Answer one invitation for exactly `selection`, the account shown when
    /// it was chosen: accept joins it, decline leaves it. The answer is the
    /// core's single fenced request for that room alone; it is not sent when
    /// the room is no longer an invitation, or when the selection or `fence`
    /// (the view it was chosen in, a security step, the app still running)
    /// ended first. `Err` means nothing was applied; once the request was
    /// sent, the core's answer is returned whatever follows (see
    /// `AccountManager::respond_invitation_selected`).
    ///
    /// The account transition is waited for at most
    /// `INVITE_TRANSITION_WAIT`, past which nothing is sent
    /// (`AccountAccessBusy`), and is held while the answer is checked, not
    /// across its network waits; it is taken again, as bounded, to record
    /// the result and read the lists again. That bookkeeping never takes or
    /// invalidates a navigation, so a room chosen meanwhile keeps its own.
    /// An accepted room or Space is opened only through `open_answered`:
    /// sync already brought the join here, the navigation `opens` captured at
    /// the click is still the current one, and its owned authority (session,
    /// exact selection, security gate, no protected surface entered since)
    /// still holds at the commit and at the snapshot's admission (a switch
    /// from A to B and back to A included). A withdrawn open changes only
    /// what is shown: the answer, its guard and its outcome stand. A failed
    /// refresh only leaves the lists as they were until the next sync. An
    /// answer already sent is not recalled.
    pub async fn respond_to_invite(
        self: &Arc<Self>,
        selection: &AccountSelection,
        room_id: &str,
        accept: bool,
        fence: &impl AnswerFence,
        opens: Option<OwnedOpen>,
        ui: Weak<AppWindow>,
    ) -> Result<InvitationAnswer, MatrixCoreError> {
        let Some(transition) = self.bounded_transition(INVITE_TRANSITION_WAIT).await else {
            return Err(MatrixCoreError::AccountAccessBusy);
        };
        if !selection.is_current() {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        let account_id = selection.account_id();
        // The invitation's name, read before joining, for an accepted Space
        // that sync has not brought into the local room list yet; past the
        // bound its room ID stands in.
        let invite = if accept {
            let invites = self.manager.invites_snapshot();
            let invite = match tokio::time::timeout(INVITE_TRANSITION_WAIT, invites).await {
                Ok(invites) => invites?.into_iter().find(|room| room.room_id == room_id),
                Err(_) => None,
            };
            if !selection.is_current() {
                return Err(MatrixCoreError::SelectedAccountChanged);
            }
            invite
        } else {
            None
        };
        // Not held across the answer's network waits: the core fences the
        // hand-off to this exact selection and `fence` itself.
        drop(transition);
        let answer = self
            .manager
            .respond_invitation_selected(selection, room_id, accept, fence)
            .await?;
        if !selection.is_current() {
            return Ok(answer);
        }
        let joined = accept && answer.outcome != InvitationOutcome::Unconfirmed;
        // Bookkeeping only, under the transition: an accepted Space's name,
        // and the lists read again. No navigation is taken or invalidated
        // here, so a room chosen since the answer keeps its own.
        {
            let Some(_transition) = self.bounded_transition(INVITE_TRANSITION_WAIT).await else {
                return Ok(answer);
            };
            if !selection.is_current() {
                return Ok(answer);
            }
            // The kind the core read from the invite state when it admitted
            // the answer: the joined room may not show it here yet.
            if joined && answer.is_space {
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
            self.invalidate_space_scan().await;
            if selection.is_current() && self.refresh(ui.clone()).await.is_ok() {
                tokio::spawn(self.clone().scan_spaces(ui.clone()));
            }
        }
        // A room still reading as an invitation locally is not opened.
        if joined && answer.outcome == InvitationOutcome::Settled {
            self.open_answered(selection, room_id, answer.is_space, opens, ui)
                .await;
        }
        Ok(answer)
    }

    /// The account transition, waited for at most `bound`.
    async fn bounded_transition(&self, bound: Duration) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        tokio::time::timeout(bound, self.account_transition.lock())
            .await
            .ok()
    }

    /// Open an accepted invitation that sync already brought here, as a Home
    /// card's accept or the center's Open asks (`open`, owned from its click
    /// by `selection`): only while the open's own authority holds (the
    /// session live, that selection current, the security gate where it was
    /// and no protected surface entered since: each entry withdrew it for
    /// good) and the navigation it was chosen in is still the current one,
    /// taken in one step under that authority's lock (`claim_navigation_from`),
    /// and only through the owned reveal of a room (reveal_room_for: the
    /// current Space when it lists the room, else the Space that does, else
    /// Home) or of a Space (reveal_space_for). That same authority is asked
    /// again at the synchronous commit and the final snapshot admission: a
    /// security step begun since, even one already ended, or a protected
    /// surface entered since stops it there, and an unshown commit is given
    /// back. A newer navigation is never taken or undone. Callers:
    /// respond_to_invite and InvitationsCenter's Open.
    pub(crate) async fn open_answered(
        self: &Arc<Self>,
        selection: &AccountSelection,
        room_id: &str,
        is_space: bool,
        open: Option<OwnedOpen>,
        ui: Weak<AppWindow>,
    ) {
        let Some(open) = open else {
            return;
        };
        if !selection.is_current() {
            return;
        }
        // A withdrawal takes the same lock: once it returns, nothing claims.
        let epoch = {
            let Some(_admitted) = open.authority.commit_guard() else {
                return;
            };
            self.claim_navigation_from(open.navigation)
        };
        let Some(epoch) = epoch else {
            return;
        };
        let (selection, room_id) = (selection.clone(), room_id.to_owned());
        let authority = open.authority;
        let _ = if is_space {
            self.reveal_space_for(selection, room_id, epoch, authority, ui)
                .await
        } else {
            self.reveal_room_for(selection, room_id, epoch, authority, ui)
                .await
        };
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

    /// Open a room chosen from search results or an activation, revealing it
    /// first: the current Space when it lists the room, otherwise the Space
    /// that does, otherwise Home. Every step is fenced by the original
    /// account `selection`, the navigation epoch and `authority`, which the
    /// requester may withdraw at any time: before the commit nothing
    /// changes; after it, the snapshot that would show the room is applied
    /// only while all three still hold (checked on the UI thread, where
    /// withdrawal happens), and an unshown commit gives back the previous
    /// room and Space. A newer navigation keeps its own epoch and state.
    /// Every wait before the admission gives way to a withdrawal of
    /// `authority`. The transition lock is released before the follow-ups
    /// (members, the feed, recovery: after_snapshot, each scoped to this
    /// snapshot) and before the reopen of an unshown reveal, so neither holds
    /// up navigation; the result still comes after them.
    pub(crate) async fn reveal_room_for(
        self: &Arc<Self>,
        selection: AccountSelection,
        room_id: String,
        navigation_epoch: u64,
        authority: NavigationAuthority,
        ui: Weak<AppWindow>,
    ) -> Result<bool, MatrixCoreError> {
        let revealed = match unless_withdrawn(&authority, self.account_transition.lock()).await {
            Some(_transition) => {
                let epoch = navigation_epoch;
                self.reveal_room_locked(&selection, room_id, epoch, &authority, None, &ui)
                    .await
            }
            None => Ok(None),
        };
        match revealed {
            Ok(Some(after)) => {
                self.after_snapshot(after, ui).await;
                Ok(true)
            }
            other => {
                self.reopen_unrevealed(&selection, navigation_epoch, ui)
                    .await;
                other.map(|_| false)
            }
        }
    }

    /// reveal_room_for under the transition lock: the guarded part only
    /// (commit, admission, give-back). `Some` with the follow-ups once its
    /// snapshot was admitted, for the caller to run (after_snapshot).
    /// `admitted`, when given, is told the moment the snapshot is admitted
    /// (see RevealOwner). Every wait before that gives way to a withdrawal of
    /// `authority`.
    async fn reveal_room_locked(
        self: &Arc<Self>,
        selection: &AccountSelection,
        room_id: String,
        navigation_epoch: u64,
        authority: &NavigationAuthority,
        admitted: Option<tokio::sync::oneshot::Sender<()>>,
        ui: &Weak<AppWindow>,
    ) -> Result<Option<AfterSnapshot>, MatrixCoreError> {
        let account_id = selection.account_id().to_owned();
        if authority.withdrawn()
            || !selection.is_current()
            || !self.navigation_is_current(navigation_epoch)
            || !matches!(
                unless_withdrawn(authority, self.require_active_account(&account_id)).await,
                Some(Ok(()))
            )
        {
            return Ok(None);
        }
        // From before the rooms and the Space graph are read until the
        // snapshot is decided, no other refresh samples the selection, so
        // none can show the room unguarded. Every wait is before the commit:
        // a withdrawal stops it at once.
        let Some(barrier) = unless_withdrawn(authority, self.snapshot_barrier.write()).await else {
            return Ok(None);
        };
        let Some(rooms) = unless_withdrawn(authority, self.manager.rooms_snapshot()).await else {
            return Ok(None);
        };
        let rooms = rooms?;
        let Some(room) = rooms.iter().find(|room| room.room_id == room_id) else {
            return Err(MatrixCoreError::RoomNotFound(room_id));
        };
        if room.is_space || room.membership != RoomMembership::Joined {
            return Err(MatrixCoreError::RoomNotFound(room_id));
        }
        // The same visibility rules as refresh: a Space is shown only while it
        // is joined or was just created (known for 60 s), and Home lists
        // direct rooms and rooms outside every scanned Space.
        let known = async {
            let mut known = HashSet::new();
            if let Some(created) = self.known_spaces.lock().await.get(&account_id) {
                for (space, value) in created {
                    if value.recorded_at.elapsed() < Duration::from_secs(60) {
                        known.insert(space.clone());
                    }
                }
            }
            known
        };
        let Some(known) = unless_withdrawn(authority, known).await else {
            return Ok(None);
        };
        let shown = |space: &str| {
            known.contains(space)
                || rooms.iter().any(|room| {
                    room.is_space
                        && room.room_id == space
                        && room.membership == RoomMembership::Joined
                })
        };
        let children = async { self.space_children.lock().await.clone() };
        let Some(space_children) = unless_withdrawn(authority, children).await else {
            return Ok(None);
        };
        let spaces = space_children
            .iter()
            .filter(|(space, children)| children.contains(&room_id) && shown(space.as_str()))
            .map(|(space, _)| space.clone())
            .collect::<Vec<_>>();
        let listed = space_children
            .values()
            .any(|children| children.contains(&room_id));
        let in_home = room.is_direct || !listed;
        // Listed only under Spaces that are not shown: visible nowhere.
        if !in_home && spaces.is_empty() {
            return Err(MatrixCoreError::RoomNotFound(room_id));
        }
        if !selection.is_current() {
            return Ok(None);
        }
        // Its only waits are its locks, before the synchronous commit itself.
        let commit = self.commit_reveal(&room_id, &spaces, in_home, navigation_epoch, authority);
        let Some(Some(previous)) = unless_withdrawn(authority, commit).await else {
            return Ok(None);
        };
        // The feed is not cleared here: the owned snapshot's follow-ups retire
        // it once the room is shown; an unshown reveal hands it to
        // reopen_unrevealed.
        let gate = SnapshotGate {
            authority: authority.clone(),
            selection: selection.clone(),
            navigation: navigation_epoch,
        };
        let mut owner = RevealOwner {
            gate,
            room_id: room_id.clone(),
            space_id: None,
            _barrier: Some(barrier),
            admitted,
        };
        let shown = self.snapshot_owned(ui.clone(), Some(&mut owner)).await;
        match shown {
            // The room's drafts are recovered once it is shown, while it is
            // still the room shown under this snapshot (after_snapshot).
            Ok(Some(mut after)) => {
                after.recover = true;
                Ok(Some(after))
            }
            other => {
                self.settle_unshown_reveal(&room_id, previous, owner._barrier.take())
                    .await;
                other
            }
        }
    }

    /// A committed reveal whose snapshot was not applied (withdrawn,
    /// replaced, its account changed, its room no longer visible there, or
    /// its refresh failed): give back the previous room and Space while they
    /// are still this reveal's, so a newer navigation's state is never
    /// undone, and only then release the barrier, so no other refresh ever
    /// samples the revealed room.
    async fn settle_unshown_reveal(
        &self,
        room_id: &str,
        previous: (Option<String>, Option<String>),
        barrier: Option<tokio::sync::RwLockWriteGuard<'_, ()>>,
    ) {
        self.restore_reveal(room_id, previous).await;
        #[cfg(test)]
        self.pauses.reach("reveal.before_release").await;
        drop(barrier);
    }

    /// Select the joined Space `space_id` as an owned reveal, the way
    /// reveal_room_for opens a room: committed exactly under `authority`
    /// (its withdrawal and its own conditions) and the navigation epoch,
    /// shown only through its own admitted snapshot, given back otherwise
    /// (reveal_joined_space_locked, the joined-Space RevealOwner barrier).
    /// Callers: open_answered (an accepted Space invitation's own open).
    /// As in reveal_room_for, every wait before the admission gives way to a
    /// withdrawal of `authority`, and the transition lock is released before
    /// the follow-ups (after_snapshot) and the reopen of an unshown reveal.
    pub(crate) async fn reveal_space_for(
        self: &Arc<Self>,
        selection: AccountSelection,
        space_id: String,
        navigation_epoch: u64,
        authority: NavigationAuthority,
        ui: Weak<AppWindow>,
    ) -> Result<bool, MatrixCoreError> {
        let revealed = match unless_withdrawn(&authority, self.account_transition.lock()).await {
            Some(_transition) => {
                let (epoch, space) = (navigation_epoch, space_id);
                self.reveal_joined_space_locked(&selection, space, epoch, &authority, None, &ui)
                    .await
            }
            None => Ok(None),
        };
        match revealed {
            Ok(Some(after)) => {
                self.after_snapshot(after, ui).await;
                Ok(true)
            }
            other => {
                self.reopen_unrevealed(&selection, navigation_epoch, ui)
                    .await;
                other.map(|_| false)
            }
        }
    }

    /// The joined-Space counterpart of reveal_room_locked (a joined link's
    /// Space, a created Space, an accepted Space invitation). Keep the same
    /// snapshot barrier until this exact selection/authority has shown its
    /// Space, or restore only this unshown commit before releasing it.
    /// `Some` with the follow-ups once its snapshot was admitted; `admitted`,
    /// when given, is told at that moment. Every wait before the commit or
    /// the admission gives way to a withdrawal of `authority`.
    async fn reveal_joined_space_locked(
        self: &Arc<Self>,
        selection: &AccountSelection,
        space_id: String,
        navigation: u64,
        authority: &NavigationAuthority,
        admitted: Option<tokio::sync::oneshot::Sender<()>>,
        ui: &Weak<AppWindow>,
    ) -> Result<Option<AfterSnapshot>, MatrixCoreError> {
        if authority.withdrawn()
            || !selection.is_current()
            || !self.navigation_is_current(navigation)
            || !matches!(
                unless_withdrawn(
                    authority,
                    self.require_active_account(selection.account_id())
                )
                .await,
                Some(Ok(()))
            )
        {
            return Ok(None);
        }
        let Some(barrier) = unless_withdrawn(authority, self.snapshot_barrier.write()).await else {
            return Ok(None);
        };
        let Some(rooms) = unless_withdrawn(authority, self.manager.rooms_snapshot()).await else {
            return Ok(None);
        };
        let rooms = rooms?;
        if !rooms.iter().any(|room| {
            room.room_id == space_id && room.is_space && room.membership == RoomMembership::Joined
        }) {
            return Err(MatrixCoreError::RoomNotFound(space_id));
        }
        // Its only waits are its locks, before the synchronous commit itself.
        let commit = self.commit_joined_space(selection, &space_id, navigation, authority);
        let Some(Some(previous)) = unless_withdrawn(authority, commit).await else {
            return Ok(None);
        };
        let mut owner = RevealOwner {
            gate: SnapshotGate {
                authority: authority.clone(),
                selection: selection.clone(),
                navigation,
            },
            room_id: String::new(),
            space_id: Some(space_id.clone()),
            _barrier: Some(barrier),
            admitted,
        };
        let shown = self.snapshot_owned(ui.clone(), Some(&mut owner)).await;
        if !matches!(shown, Ok(Some(_))) {
            // The transition and barrier still exclude another navigation's
            // commit. Check the installed Space under both selection locks;
            // never replace any selection that is no longer this commit's.
            let mut space = self.selected_space.lock().await;
            let mut room = self.selected_room.lock().await;
            if space.as_deref() == Some(space_id.as_str()) {
                *space = previous.0;
                *room = previous.1;
            }
            drop(room);
            drop(space);
            drop(owner._barrier.take());
        }
        shown
    }

    async fn commit_joined_space(
        &self,
        selection: &AccountSelection,
        space_id: &str,
        navigation: u64,
        authority: &NavigationAuthority,
    ) -> Option<(Option<String>, Option<String>)> {
        let mut space = self.selected_space.lock().await;
        let mut room = self.selected_room.lock().await;
        let _admitted = authority.commit_guard()?;
        if !selection.is_current() || !self.navigation_is_current(navigation) {
            return None;
        }
        let previous = (space.clone(), room.clone());
        *space = Some(space_id.to_owned());
        *room = None;
        Some(previous)
    }

    /// A reveal that showed nothing after navigate() moved the navigation
    /// epoch for it. The open room's feed (live or focused) publishes only
    /// under the epoch it was opened with, so while this reveal's selection
    /// and epoch are still current (it was withdrawn or failed and nothing
    /// newer navigated), retire that feed and refresh: open_room reopens the
    /// room's live feed under the current epoch. The open thread is kept. A
    /// newer navigation or account shows its own room instead.
    async fn reopen_unrevealed(
        self: &Arc<Self>,
        selection: &AccountSelection,
        navigation_epoch: u64,
        ui: Weak<AppWindow>,
    ) {
        if !selection.is_current() || !self.navigation_is_current(navigation_epoch) {
            return;
        }
        let expected_room = { self.selected_room.lock().await.clone() };
        {
            // The same order as the feed installs: key, then selected room.
            let mut key = self.feed_key.lock().await;
            let selected = self.selected_room.lock().await;
            // Either wait may outlive a newer room or account choice: recheck
            // at the mutation, with no suspension after this check.
            if !self.is_live.load(Ordering::Acquire)
                || !selection.is_current()
                || !self.navigation_is_current(navigation_epoch)
                || *selected != expected_room
            {
                return;
            }
            *key = None;
        }
        let _ = self.refresh(ui).await;
    }

    /// Select `room_id` and, when the current Space does not list it, the
    /// lowest-ID Space that does (or Home), in one step, only while
    /// `authority` holds and the navigation is current. Withdrawal takes the
    /// same lock, so a withdrawn reveal changes nothing. Returns the previous
    /// Space and room.
    async fn commit_reveal(
        &self,
        room_id: &str,
        spaces: &[String],
        in_home: bool,
        navigation_epoch: u64,
        authority: &NavigationAuthority,
    ) -> Option<(Option<String>, Option<String>)> {
        let mut space = self.selected_space.lock().await;
        let mut room = self.selected_room.lock().await;
        let _admitted = authority.commit_guard()?;
        if !self.navigation_is_current(navigation_epoch) {
            return None;
        }
        let previous = (space.clone(), room.clone());
        let visible = match &*space {
            Some(current) => spaces.contains(current),
            None => in_home,
        };
        if !visible {
            *space = if in_home {
                None
            } else {
                spaces.iter().min().cloned()
            };
        }
        *room = Some(room_id.to_owned());
        Some(previous)
    }

    /// Undo a committed reveal whose room has not been shown.
    async fn restore_reveal(&self, room_id: &str, previous: (Option<String>, Option<String>)) {
        let mut space = self.selected_space.lock().await;
        let mut room = self.selected_room.lock().await;
        if room.as_deref() == Some(room_id) {
            let (previous_space, previous_room) = previous;
            *space = previous_space;
            *room = previous_room;
        }
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
        self.refresh_owned(ui).await.map(|_| ())
    }

    /// One refresh: its snapshot, then its follow-ups in place, as before.
    /// Whether a snapshot was queued. Owned reveals use snapshot_owned and
    /// run the follow-ups themselves (after_snapshot).
    async fn refresh_owned(self: &Arc<Self>, ui: Weak<AppWindow>) -> Result<bool, MatrixCoreError> {
        match self.snapshot_owned(ui.clone(), None).await? {
            Some(after) => {
                self.after_snapshot(after, ui).await;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Build and queue one snapshot of the active account; `Some` with what
    /// it leaves to do once it was queued (with `owner`, once applied).
    ///
    /// `owner`: the reveal this snapshot publishes for, holding the snapshot
    /// barrier. Its snapshot is applied only while the reveal's gate admits
    /// it on the UI thread, which claims the admission on its authority;
    /// only then is the barrier released. Every wait before that decision
    /// gives way to a withdrawal of that authority (`within`), so a
    /// withdrawn reveal never holds its barrier or the transition lock for
    /// it, and nothing it read is shown. Withdrawn while the snapshot waits
    /// on the UI thread, it stops waiting unless the UI already claimed it.
    /// Every other refresh waits at the barrier before it samples anything.
    async fn snapshot_owned(
        self: &Arc<Self>,
        ui: Weak<AppWindow>,
        mut owner: Option<&mut RevealOwner<'_>>,
    ) -> Result<Option<AfterSnapshot>, MatrixCoreError> {
        let barrier = if owner.is_some() {
            None
        } else {
            Some(self.snapshot_barrier.read().await)
        };
        let stop = owner.as_ref().map(|owner| owner.gate.authority.clone());
        let stop = stop.as_ref();
        #[cfg(test)]
        self.pauses.reach("refresh.after_barrier").await;
        if !self.is_live.load(Ordering::Acquire) {
            return Ok(None);
        }
        let render_epoch = self.render_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let navigation_epoch = self.navigation_epoch.load(Ordering::Acquire);
        let Some(accounts) = within(stop, self.manager.accounts()).await else {
            return Ok(None);
        };
        let Some(active) = within(stop, self.manager.active_account()).await else {
            return Ok(None);
        };
        let active = active.ok_or(MatrixCoreError::NoActiveAccount)?;
        // A reveal's commit is held until its snapshot is decided: a
        // withdrawal stops this read at once, before anything is queued, and
        // the reveal gives its commit back.
        let Some(rooms) = within(stop, self.manager.rooms_snapshot()).await else {
            return Ok(None);
        };
        let rooms = rooms?;
        let Some(still) = within(stop, self.manager.active_account()).await else {
            return Ok(None);
        };
        if still.as_ref().map(|account| &account.id) != Some(&active.id)
            || self.render_epoch.load(Ordering::Acquire) != render_epoch
        {
            return Ok(None);
        }
        let known_spaces = {
            let Some(mut by_account) = within(stop, self.known_spaces.lock()).await else {
                return Ok(None);
            };
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
        // Each guard is dropped as soon as its value is read.
        let Some(selected_space) =
            within(stop, async { self.selected_space.lock().await.clone() }).await
        else {
            return Ok(None);
        };
        let selected_space = selected_space.filter(|space_id| {
            known_spaces.contains_key(space_id)
                || rooms.iter().any(|room| {
                    room.is_space
                        && room.room_id == *space_id
                        && room.membership == RoomMembership::Joined
                })
        });
        if self.render_epoch.load(Ordering::Acquire) != render_epoch {
            return Ok(None);
        }
        // A reveal writes its Space only once its room is confirmed below.
        if owner.is_none() {
            *self.selected_space.lock().await = selected_space.clone();
        }
        let Some(space_children) =
            within(stop, async { self.space_children.lock().await.clone() }).await
        else {
            return Ok(None);
        };
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
        let Some(current) = within(stop, async { self.selected_room.lock().await.clone() }).await
        else {
            return Ok(None);
        };
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
            return Ok(None);
        }
        // A reveal shows exactly its room. When the room is not visible under
        // the committed Space any more (the Space graph or membership changed
        // since the reveal read it), nothing is written, so the reveal gives
        // the previous room and Space back instead of showing another room.
        if let Some(owner) = owner.as_ref() {
            if let Some(space) = &owner.space_id {
                if selected_space.as_deref() != Some(space.as_str()) {
                    return Err(MatrixCoreError::RoomNotFound(space.clone()));
                }
            } else if selected.as_deref() != Some(owner.room_id.as_str()) {
                return Err(MatrixCoreError::RoomNotFound(owner.room_id.clone()));
            }
            let Some(mut space) = within(stop, self.selected_space.lock()).await else {
                return Ok(None);
            };
            *space = selected_space.clone();
        }
        let Some(mut room) = within(stop, self.selected_room.lock()).await else {
            return Ok(None);
        };
        *room = selected.clone();
        drop(room);
        // A reveal's draft error is shown only with its snapshot, in its own
        // navigation (after_snapshot).
        let mut draft_error = None;
        if let Some(room_id) = selected.as_deref() {
            let loading = async {
                // Owned reveals only: an ordinary refresh never takes it.
                #[cfg(test)]
                if stop.is_some() {
                    self.pauses.reach("snapshot.draft").await;
                }
                self.drafts
                    .load_if_missing(&self.manager, &active.id, room_id)
                    .await
            };
            let Some(loaded) = within(stop, loading).await else {
                return Ok(None);
            };
            if let Err(error) = loaded {
                let message = format!("Could not restore draft on this device: {error}");
                if owner.is_some() {
                    draft_error = Some(message);
                } else {
                    show_error(&ui, message);
                }
            }
        }
        if self.render_epoch.load(Ordering::Acquire) != render_epoch {
            return Ok(None);
        }
        let member_account_id = active.id.clone();
        let marker_rooms = rooms
            .iter()
            .filter(|room| room.membership == RoomMembership::Joined && !room.is_space)
            .map(|room| room.room_id.clone())
            .collect::<Vec<_>>();
        let local_voice_rooms = self.voice_spaces.local_rooms(&member_account_id);
        let shared = self.voice_spaces.shared_rooms(&member_account_id);
        let Some(shared_voice_rooms) = within(stop, shared).await else {
            return Ok(None);
        };
        let view = Snapshot {
            accounts,
            active,
            rooms,
            selected: selected.clone(),
            selected_space,
            known_spaces,
            space_children,
            all_space_children,
            local_voice_rooms,
            shared_voice_rooms,
        };
        let current_epoch = self.render_epoch.clone();
        let is_live = self.is_live.clone();
        let drafts = self.drafts.clone();
        let security_gate = self.security_gate.clone();
        let voice_spaces = self.voice_spaces.clone();
        let gate = owner.as_ref().map(|owner| owner.gate.clone());
        let navigation_now = self.navigation_epoch.clone();
        let thread_now = self.thread_generation.clone();
        let (decide, mut decision) = tokio::sync::oneshot::channel();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            let owned = gate.is_some();
            // The gate claims an owned snapshot's admission last; the apply
            // below runs outside every lock.
            let shown = is_live.load(Ordering::Acquire)
                && current_epoch.load(Ordering::Acquire) == render_epoch
                && gate.is_none_or(|gate| gate.admits_on(&ui, &navigation_now));
            // A shown reveal ends the left room's thread requests here, with
            // its snapshot; one opened in the shown room from now on is newer.
            let thread = (owned && shown).then(|| thread_now.fetch_add(1, Ordering::AcqRel) + 1);
            if shown {
                apply_snapshot(&ui, view, &drafts, &security_gate, &voice_spaces);
            }
            let _ = decide.send(thread);
        });
        drop(barrier);
        let mut after = AfterSnapshot {
            account_id: member_account_id,
            room_id: selected,
            selection: None,
            navigation_epoch,
            marker_rooms,
            owned: None,
            retire_feed: false,
            recover: false,
        };
        let Some(owner) = owner.as_mut() else {
            after.selection = self.manager.account_selection(&after.account_id);
            return Ok(Some(after));
        };
        // Not applied, or dropped unsent with a closed window: nothing of the
        // room is loaded, and the reveal still holds the barrier. Withdrawn
        // (or its own conditions failed) before the UI decided: that is made
        // final under the authority's lock (withdraw), linearized with the
        // UI's claim. A claim that came first is shown or being applied, and
        // its decision follows; otherwise nothing is ever shown.
        let authority = owner.gate.authority.clone();
        let decided = match unless_withdrawn(&authority, &mut decision).await {
            Some(decided) => decided,
            None if authority.withdraw() => decision.await,
            None => return Ok(None),
        };
        let Ok(Some(thread)) = decided else {
            return Ok(None);
        };
        // The admission milestone: shown now, whatever the follow-ups come
        // to. The barrier goes with it; the caller runs the follow-ups.
        if let Some(admitted) = owner.admitted.take() {
            let _ = admitted.send(());
        }
        owner._barrier = None;
        // A reveal's room opens under the selection its snapshot was shown
        // for, never one looked up again.
        after.selection = Some(owner.gate.selection.clone());
        after.owned = Some((thread, draft_error));
        after.retire_feed = owner.space_id.is_some();
        Ok(Some(after))
    }

    /// What a snapshot leaves to do once shown (an owned reveal: admitted),
    /// each step scoped to exactly that snapshot (see AfterSnapshot): for an
    /// owned reveal the left room's thread, the feed slot and the draft
    /// error; then the room's members and live feed, recovery, and the
    /// voice markers. Every step rechecks that its snapshot is still the
    /// current one where it writes, so a later navigation, account switch or
    /// lock keeps its own feed, key, draft, presentation and selection. A
    /// search result's or a created room's reveal runs this with no
    /// transition lock and no barrier held, so none of these waits holds up
    /// navigation; a join's open runs it under its own transition lock, as
    /// before.
    async fn after_snapshot(self: &Arc<Self>, after: AfterSnapshot, ui: Weak<AppWindow>) {
        let AfterSnapshot {
            account_id,
            room_id,
            selection,
            navigation_epoch,
            marker_rooms,
            owned,
            retire_feed,
            recover,
        } = after;
        let current = |live: &Self| {
            live.navigation_is_current(navigation_epoch)
                && selection.as_ref().is_some_and(AccountSelection::is_current)
        };
        if let Some((thread, draft_error)) = owned {
            // The left room's thread ends, unless one was opened since.
            self.clear_thread_if_current(thread).await;
            // Any feed in the slot was opened under an epoch navigate() has
            // replaced and can no longer publish: open_room opens the room's
            // feed under this reveal's epoch. A newer navigation keeps its own
            // key and feed.
            let reset = {
                #[cfg(test)]
                self.pauses.reach("snapshot.before_feed_key").await;
                // The order of the feed installs: key, feed, selected room.
                let mut key = self.feed_key.lock().await;
                let mut feed = self.feed.lock().await;
                let selected = self.selected_room.lock().await;
                if self.is_live.load(Ordering::Acquire) && current(self) && *selected == room_id {
                    *key = None;
                    // A joined Space may show no room: its admitted empty
                    // snapshot retires the left room's feed too, after the UI
                    // has captured its draft and accepted this selection.
                    if retire_feed && room_id.is_none() {
                        self.generation.fetch_add(1, Ordering::AcqRel);
                        self.recovery_notices
                            .clear(crate::timeline_recovery::Pane::Room);
                        self.drop_stale_focus();
                        if let Some(task) = feed.take() {
                            task.abort();
                        }
                    }
                    true
                } else {
                    false
                }
            };
            if reset && let (Some(message), Some(selection)) = (draft_error, selection.clone()) {
                self.show_navigation_error(
                    &ui,
                    selection,
                    navigation_epoch,
                    room_id.clone(),
                    message,
                );
            }
        }
        if let Some(room_id) = room_id.clone() {
            // The snapshot's member load carries its selection and navigation
            // to the load's own claim (MemberOwner). Without a selection (the
            // account is restoring, switching or logging out) there is no
            // owner and no member load: the next admitted snapshot loads.
            if let Some(selection) = selection.clone() {
                let owner = MemberOwner {
                    selection,
                    navigation_epoch,
                };
                let (account, room) = (account_id.clone(), room_id.clone());
                self.refresh_members_owned(account, room, ui.clone(), false, owner)
                    .await;
            }
            if let Some(selection) = selection.clone() {
                self.open_room(selection, room_id, navigation_epoch, ui.clone())
                    .await;
            }
        }
        // A revealed room's drafts are recovered for it while it is still the
        // room shown under that snapshot's selection and navigation.
        if recover && let (Some(selection), Some(room_id)) = (selection.clone(), room_id) {
            let still_shown = self.selected_room.lock().await.as_deref() == Some(room_id.as_str());
            if still_shown && current(self) {
                self.complete_room_recovery(selection, room_id, ui.clone())
                    .await;
            }
        }
        if let Some(selection) = selection {
            self.marker_scans.queue(
                Arc::downgrade(self),
                &Handle::current(),
                crate::marker_scans::Request {
                    selection,
                    rooms: marker_rooms,
                    ui,
                },
            );
        }
    }

    pub(crate) async fn drain_marker_scans(
        &self,
    ) -> Result<(), crate::marker_scans::MarkerDrainError> {
        self.marker_scans.drain().await
    }

    pub(crate) async fn run_marker_scan(
        self: &Arc<Self>,
        request: crate::marker_scans::Request,
        authority: crate::marker_scans::Authority,
    ) {
        if !self.is_live.load(Ordering::Acquire)
            || self.action_admission_closed()
            || !authority.is_current()
        {
            return;
        }
        if self
            .voice_spaces
            .scan_shared_owned(&self.manager, &authority, &request.rooms)
            .await
        {
            if !authority.is_current() {
                return;
            }
            let shared_rooms = self.voice_spaces.shared_rooms(authority.account_id()).await;
            let live = Arc::downgrade(self);
            let _ = request.ui.upgrade_in_event_loop(move |window| {
                let Some(live) = live.upgrade() else {
                    return;
                };
                if authority.is_current()
                    && live.is_live.load(Ordering::Acquire)
                    && !live.action_admission_closed()
                    && !window.get_application_closing()
                    && window.get_active_account_local_id().as_str() == authority.account_id()
                {
                    apply_shared_voice_labels(&window, &shared_rooms);
                }
            });
        }
    }

    /// Publish an error only in the exact navigation it belongs to. The
    /// event-loop queue is another asynchronous boundary, so the worker's
    /// earlier checks cannot authorize publication in a newer room or
    /// account: they are checked again in the UI callback.
    fn show_navigation_error(
        &self,
        ui: &Weak<AppWindow>,
        selection: AccountSelection,
        navigation_epoch: u64,
        room_id: Option<String>,
        error: String,
    ) {
        let navigation = self.navigation_epoch.clone();
        let is_live = self.is_live.clone();
        let _ = ui.upgrade_in_event_loop(move |window| {
            if is_live.load(Ordering::Acquire)
                && selection.is_current()
                && navigation.load(Ordering::Acquire) == navigation_epoch
                && visible_feed_target(
                    window.get_active_account_local_id().as_str(),
                    window.get_active_room_id().as_str(),
                    selection.account_id(),
                    room_id.as_deref().unwrap_or(""),
                )
            {
                window.set_session_error(error.into());
            }
        });
    }

    /// Fetch members for the visible account and room. Sync can trigger many
    /// presentation refreshes, so refresh at most once per short interval
    /// unless the user explicitly requests it. Callers: the member list's
    /// Refresh and moderation's settled action (both forced, for the room on
    /// screen); after_snapshot uses refresh_members_owned.
    ///
    /// The load's owner is captured here, synchronously, before the first
    /// await: `account_id`'s exact current selection (none when another
    /// account is active, or while one is restoring, switching or logging
    /// out) and the navigation now. A load with no owner does nothing, so a
    /// forced Refresh or moderation refresh for account A can never claim the
    /// member epoch, or publish, after B became active, even in a room with
    /// the same ID.
    pub async fn refresh_members_for(
        self: &Arc<Self>,
        account_id: String,
        room_id: String,
        ui: Weak<AppWindow>,
        force: bool,
    ) {
        let navigation_epoch = self.navigation_epoch.load(Ordering::Acquire);
        let Some(selection) = self.manager.account_selection(&account_id) else {
            return;
        };
        let owner = MemberOwner {
            selection,
            navigation_epoch,
        };
        self.refresh_members_owned(account_id, room_id, ui, force, owner)
            .await;
    }

    /// refresh_members_for, for `owner`, captured before any await (every
    /// forced and automatic load has one; there is no owner-free load). The
    /// member epoch is claimed under member_request only while the room is
    /// still the one selected and the owner's exact selection (of
    /// `account_id`) and navigation are still current: checked before the
    /// first await and again after every await, where the claim happens. A
    /// load that fails them installs no epoch or request and touches no
    /// loading state, members or error, so a newer navigation's or another
    /// account's load keeps its own. Lock order at the claim: member_request,
    /// then selected_room; no manager or network wait is held under either.
    async fn refresh_members_owned(
        self: &Arc<Self>,
        account_id: String,
        room_id: String,
        ui: Weak<AppWindow>,
        force: bool,
        owner: MemberOwner,
    ) {
        let owned = |owner: &MemberOwner| {
            owner.selection.is_current()
                && owner.selection.account_id() == account_id
                && self.navigation_is_current(owner.navigation_epoch)
        };
        if !owned(&owner)
            || !self.is_live.load(Ordering::Acquire)
            || self.selected_room.lock().await.as_deref() != Some(room_id.as_str())
            || self
                .manager
                .active_account()
                .await
                .is_none_or(|account| account.id != account_id)
        {
            return;
        }
        #[cfg(test)]
        self.pauses.reach("members.before_claim").await;
        let epoch = {
            let mut request = self.member_request.lock().await;
            // The claim, after the awaits above: the earlier room check may
            // be stale by now.
            let selected = self.selected_room.lock().await.as_deref() == Some(room_id.as_str());
            // The original owner, never one looked up again: a load whose
            // account is no longer the selected one claims nothing, even
            // where another account now shows a room with the same ID.
            if !self.is_live.load(Ordering::Acquire) || !selected || !owned(&owner) {
                return;
            }
            let (member_epoch, navigation_epoch) = (
                self.member_epoch.load(Ordering::Acquire),
                self.navigation_epoch.load(Ordering::Acquire),
            );
            if !force
                && request.as_ref().is_some_and(|previous| {
                    previous.serves(&account_id, &room_id, member_epoch, navigation_epoch)
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
                owner: Some(owner.clone()),
            });
            epoch
        };
        // Every publication of this load carries what it claimed under.
        let publication = MemberPublication {
            epoch,
            account_id: account_id.clone(),
            room_id: room_id.clone(),
            owner: Some(owner),
            is_live: self.is_live.clone(),
            member_epoch: self.member_epoch.clone(),
            navigation_epoch: self.navigation_epoch.clone(),
        };
        // Claimed, its loading flag not yet queued: a newer load may run
        // and finish here.
        #[cfg(test)]
        self.pauses.reach("members.before_loading").await;
        let loading = publication.clone();
        let _ = ui.upgrade_in_event_loop(move |window| {
            loading.publish(&window, MemberUpdate::Loading);
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
            let update = match result {
                Ok(Ok(members)) => {
                    MemberUpdate::Members(members.iter().map(member_item).collect::<Vec<_>>())
                }
                Ok(Err(error)) => MemberUpdate::Error(crate::directory_plain_text(
                    &format!("Could not load members: {error}"),
                    240,
                )),
                Err(_) => {
                    MemberUpdate::Error("Loading members timed out. Retry to refresh.".to_owned())
                }
            };
            let _ = ui.upgrade_in_event_loop(move |window| {
                publication.publish(&window, update);
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
        let Some(selection) = self.manager.account_selection(&account.id) else {
            return;
        };
        let rooms = match selection.guard(self.manager.rooms_snapshot()).await {
            Some(Ok(rooms)) => rooms,
            None => return,
            Some(Err(error)) => {
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
            if !selection.is_current() {
                return;
            }
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
                    self.manager.space_hierarchy_selected(
                        &selection,
                        &space_id,
                        from.as_deref(),
                        100,
                    ),
                )
                .await;
                let page = match result {
                    Ok(Ok(page)) => page,
                    Ok(Err(_)) if !selection.is_current() => return,
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
        {
            // Never change the graph a deciding reveal has read.
            let _barrier = self.snapshot_barrier.read().await;
            let mut children = self.space_children.lock().await;
            // Both waits may outlive a switch or a newer scan. Revalidate at
            // the actual commit, with no suspension before installing it.
            if !selection.is_current()
                || self.space_scan_epoch.load(Ordering::Acquire) != scan_epoch
                || !self.is_live.load(Ordering::Acquire)
            {
                return;
            }
            *children = graph;
        }
        if let Err(error) = self.refresh(ui.clone()).await {
            show_error(&ui, error.to_string());
        }
    }

    async fn invalidate_space_scan(&self) {
        self.space_scan_epoch.fetch_add(1, Ordering::AcqRel);
        *self.space_scan_key.lock().await = None;
        *self.space_scan_failed_at.lock().await = None;
    }

    fn recovery_lease(
        &self,
        selection: AccountSelection,
        room: String,
        root: Option<String>,
        focused: bool,
        generation_at: u64,
        navigation_at: u64,
    ) -> crate::timeline_recovery::Lease {
        crate::timeline_recovery::Lease {
            selection,
            room,
            generation: if root.is_some() {
                self.thread_generation.clone()
            } else {
                self.generation.clone()
            },
            root,
            focused,
            navigation: self.navigation_epoch.clone(),
            navigation_at,
            generation_at,
            live: self.is_live.clone(),
            gate: self.security_gate.clone(),
        }
    }

    /// Open the live feed of `room_id` under the caller's exact `selection`.
    async fn open_room(
        self: &Arc<Self>,
        selection: AccountSelection,
        room_id: String,
        navigation_epoch: u64,
        ui: Weak<AppWindow>,
    ) {
        let account_id = selection.account_id().to_owned();
        if !selection.is_current()
            || !self.navigation_is_current(navigation_epoch)
            || self.require_active_account(&account_id).await.is_err()
            || self.selected_room.lock().await.as_deref() != Some(room_id.as_str())
        {
            return;
        }
        let requested_key = (account_id.clone(), room_id.clone());
        #[cfg(test)]
        self.pauses.reach("feed.before_claim").await;
        let generation = {
            // The same order as focused installs: key, feed, selected room.
            let key = self.feed_key.lock().await;
            let feed = self.feed.lock().await;
            let selected = self.selected_room.lock().await;
            // These waits may outlive a newer navigation: a stale open must
            // not even advance the feed generation, which would end the
            // newer navigation's running feed.
            if !self.is_live.load(Ordering::Acquire)
                || !selection.is_current()
                || !self.navigation_is_current(navigation_epoch)
                || selected.as_deref() != Some(room_id.as_str())
            {
                return;
            }
            if key.as_ref() == Some(&requested_key)
                && feed.as_ref().is_some_and(|task| !task.is_finished())
            {
                return;
            }
            let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
            self.recovery_notices
                .clear(crate::timeline_recovery::Pane::Room);
            // A live feed ends the focused view it replaces; a newer one
            // stays. No suspension through the claim and this cleanup.
            self.drop_stale_focus();
            generation
        };
        let feed = match self
            .manager
            .message_feed_selected(&selection, &room_id)
            .await
        {
            Ok(feed) => feed,
            Err(error) => {
                if selection.is_current()
                    && self.navigation_is_current(navigation_epoch)
                    && self.require_active_account(&account_id).await.is_ok()
                {
                    // Shown only in this exact navigation and room.
                    self.show_navigation_error(
                        &ui,
                        selection.clone(),
                        navigation_epoch,
                        Some(room_id.clone()),
                        error.to_string(),
                    );
                }
                return;
            }
        };
        if !selection.is_current()
            || self.generation.load(Ordering::Acquire) != generation
            || !self.navigation_is_current(navigation_epoch)
            || self.require_active_account(&account_id).await.is_err()
            || self.selected_room.lock().await.as_deref() != Some(room_id.as_str())
        {
            return;
        }
        #[cfg(test)]
        self.pauses.reach("feed.before_commit").await;
        // Loading stayed outside these locks and outside account_transition.
        // Every local wait is before the final exact ownership check; from
        // that check through publishing the task and key, nothing suspends.
        let mut key_slot = self.feed_key.lock().await;
        let mut feed_slot = self.feed.lock().await;
        let selected = self.selected_room.lock().await;
        if !self.is_live.load(Ordering::Acquire)
            || !selection.is_current()
            || self.generation.load(Ordering::Acquire) != generation
            || !self.navigation_is_current(navigation_epoch)
            || selected.as_deref() != Some(room_id.as_str())
        {
            return;
        }
        if let Some(previous) = feed_slot.take() {
            previous.abort();
        }
        *key_slot = Some(requested_key);
        let mut updates = feed.subscribe_presentation();
        let feed_generation = self.generation.clone();
        let feed_navigation_epoch = self.navigation_epoch.clone();
        let is_live = self.is_live.clone();
        // VIEWPORT LIFECYCLE: read-only, for the publication's security generation.
        let viewport_gate = self.security_gate.clone();
        self.recovery_notices
            .clear(crate::timeline_recovery::Pane::Room);
        let recovery = self.recovery_notices.clone();
        let recovery_lease = self.recovery_lease(
            selection.clone(),
            room_id.clone(),
            None,
            false,
            generation,
            navigation_epoch,
        );
        let feed_account_id = account_id;
        let feed_room_id = room_id;
        let handle = tokio::spawn(async move {
            let _feed = feed;
            let delivery = Arc::new(crate::timeline_delivery::Latest::new());
            loop {
                let presentation = updates.borrow_and_update().clone();
                #[cfg(test)]
                let observed = presentation.messages.clone();
                let security = crate::timeline_viewport::security_generation(&viewport_gate);
                if delivery.submit((presentation, security)) {
                    let ui_for_update = ui.clone();
                    let current_generation = feed_generation.clone();
                    let current_navigation_epoch = feed_navigation_epoch.clone();
                    let live_flag = is_live.clone();
                    let recovery = recovery.clone();
                    let recovery_lease = recovery_lease.clone();
                    let gate = viewport_gate.clone();
                    let account_id = feed_account_id.clone();
                    let room_id = feed_room_id.clone();
                    let pending = delivery.clone();
                    let selected_account = selection.clone();
                    if ui_for_update
                        .upgrade_in_event_loop(move |ui| {
                            let Some((presentation, security)) = pending.take() else {
                                return;
                            };
                            let messages = presentation.messages;
                            let copy_scope = crate::message_copy::capture_publication(
                                &ui,
                                &selected_account,
                                false,
                            );
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
                                publish_live_messages_scoped(
                                    &ui,
                                    messages.as_ref().clone(),
                                    copy_scope,
                                );
                                recovery.publish(
                                    &ui,
                                    crate::timeline_recovery::Pane::Room,
                                    presentation.has_undecryptable,
                                    recovery_lease,
                                    security,
                                );
                                // VIEWPORT LIFECYCLE: under the checks above.
                                crate::timeline_viewport::rows_published(
                                    &ui,
                                    &selected_account,
                                    navigation_epoch,
                                    generation,
                                    &gate,
                                );
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
        // Through the held guard: no second wait after the ownership check.
        *feed_slot = Some(handle);
        drop(selected);
    }

    pub async fn clear_feed(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.recovery_notices.clear_all();
        self.drop_stale_focus();
        if let Some(task) = self.feed.lock().await.take() {
            task.abort();
        }
        *self.feed_key.lock().await = None;
        self.clear_thread().await;
    }

    /// Held only around synchronous slot updates, never across an await.
    fn focus_slot(&self) -> std::sync::MutexGuard<'_, FocusSlot<morpheus_matrix::FocusedView>> {
        self.focused
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Sample the feed generation only while holding the slot: a commit
    /// installs its view and generation under the same lock, so a newer
    /// view committed meanwhile is never mistaken for a stale one.
    fn drop_stale_focus(&self) {
        let mut slot = self.focus_slot();
        let current = self.generation.load(Ordering::Acquire);
        slot.drop_stale(current);
    }

    /// The focused view shown in `room_id` under the current feed, for
    /// paging exactly that view. Capture it when the user acts.
    pub(crate) fn focused_view(&self, room_id: &str) -> Option<morpheus_matrix::FocusedView> {
        let view = {
            let slot = self.focus_slot();
            let current = self.generation.load(Ordering::Acquire);
            slot.view(current)
        };
        view.filter(|view| view.room_id() == room_id)
    }

    /// Show the open room's conversation around `event_id` in place of its
    /// latest messages, until the user returns to live or leaves the room.
    /// Its rows are published under the live feed's account, room, feed
    /// generation and navigation checks; while it runs, refreshes keep it
    /// (open_room sees a running feed for the room). Withdrawing `authority`
    /// (or its own conditions failing, see NavigationAuthority::holding)
    /// before the commit installs nothing; after it but before the view's
    /// first rows are shown, the view shows nothing and the room returns to
    /// its live feed. `selection` and `navigation_epoch` are the opening's
    /// own, captured when it started: a delayed focus after A -> B -> A or
    /// another navigation does nothing.
    pub(crate) async fn focus_event_for(
        self: &Arc<Self>,
        selection: AccountSelection,
        room_id: String,
        event_id: &str,
        navigation_epoch: u64,
        authority: NavigationAuthority,
        ui: Weak<AppWindow>,
    ) -> Result<bool, MatrixCoreError> {
        if authority.withdrawn()
            || !selection.is_current()
            || !self.navigation_is_current(navigation_epoch)
        {
            return Ok(false);
        }
        let account_id = selection.account_id().to_owned();
        // Load without the transition lock: account controls never wait on
        // the homeserver. Revocation of the selection ends the load. The
        // loaded view is this call's alone until the commit below.
        let (view, feed) = self
            .manager
            .focused_view_selected(&selection, &room_id, event_id, FOCUS_CONTEXT_EVENTS)
            .await?;
        #[cfg(test)]
        self.pauses.reach("focus.before_commit").await;
        // Take every lock the install needs before the commit, in open_room's
        // order (key, then feed), and check every fence after the last wait.
        // From the commit on nothing suspends, so cancelling this future
        // (the caller's timeout) never leaves a committed view without its
        // feed, nor the old feed ended for nothing.
        let _transition = self.account_transition.lock().await;
        let mut key_slot = self.feed_key.lock().await;
        let mut feed_slot = self.feed.lock().await;
        if self.require_active_account(&account_id).await.is_err()
            || self.selected_room.lock().await.as_deref() != Some(room_id.as_str())
            || !selection.is_current()
            || !self.navigation_is_current(navigation_epoch)
        {
            return Ok(false);
        }
        // The commit: from here the live feed is stale and this view owns
        // the conversation. A superseded load (its authority withdrawn by a
        // newer open) installs nothing and is dropped here.
        let committed = {
            let mut slot = self.focus_slot();
            slot.commit(&self.generation, &authority, view.clone())
        };
        let Some(generation) = committed else {
            return Ok(false);
        };
        if let Some(previous) = feed_slot.take() {
            previous.abort();
        }
        *key_slot = Some((account_id.clone(), room_id.clone()));
        let mut updates = feed.subscribe_presentation();
        let current_generation = self.generation.clone();
        let navigation_now = self.navigation_epoch.clone();
        let is_live = self.is_live.clone();
        // VIEWPORT LIFECYCLE: read-only, for the publication's security generation.
        let viewport_gate = self.security_gate.clone();
        self.recovery_notices
            .clear(crate::timeline_recovery::Pane::Room);
        let recovery = self.recovery_notices.clone();
        let recovery_lease = self.recovery_lease(
            selection.clone(),
            room_id.clone(),
            None,
            true,
            generation,
            navigation_epoch,
        );
        let feed_account_id = account_id;
        // Withdrawal still counts until the view's first rows are shown,
        // checked on the UI thread where withdraw() happens.
        let unshown = Arc::new(AtomicBool::new(true));
        let unshown_view = UnshownView {
            live: Arc::downgrade(self),
            runtime: Handle::current(),
            view: view.clone(),
            ui: ui.clone(),
        };
        let handle = tokio::spawn(async move {
            // The feed task keeps its own view alive while it publishes.
            let (_view, _feed) = (view, feed);
            let delivery = Arc::new(crate::timeline_delivery::Latest::new());
            loop {
                let presentation = updates.borrow_and_update().clone();
                let security = crate::timeline_viewport::security_generation(&viewport_gate);
                if delivery.submit((presentation, security)) {
                    let pending = delivery.clone();
                    let selected_account = selection.clone();
                    let current_generation = current_generation.clone();
                    let navigation_now = navigation_now.clone();
                    let live_flag = is_live.clone();
                    let account_id = feed_account_id.clone();
                    let room_id = room_id.clone();
                    let (unshown, authority) = (unshown.clone(), authority.clone());
                    let back = unshown_view.clone();
                    let recovery = recovery.clone();
                    let recovery_lease = recovery_lease.clone();
                    let gate = viewport_gate.clone();
                    let queued = ui.upgrade_in_event_loop(move |ui| {
                        let Some((presentation, security)) = pending.take() else {
                            return;
                        };
                        let messages = presentation.messages;
                        // Withdrawn after the commit, before any row of the
                        // view was shown (or another surface is up, which
                        // withdraws it): show none and give the room back.
                        if unshown.load(Ordering::Acquire) && !authority.admits(&ui) {
                            back.give_back(selected_account, room_id);
                            return;
                        }
                        let copy_scope =
                            crate::message_copy::capture_publication(&ui, &selected_account, false);
                        if selected_account.is_current()
                            && live_flag.load(Ordering::Acquire)
                            && current_generation.load(Ordering::Acquire) == generation
                            && navigation_now.load(Ordering::Acquire) == navigation_epoch
                            && visible_feed_target(
                                ui.get_active_account_local_id().as_str(),
                                ui.get_active_room_id().as_str(),
                                &account_id,
                                &room_id,
                            )
                        {
                            unshown.store(false, Ordering::Release);
                            ui.set_messages(crate::timeline_model::update_scoped(
                                ui.get_messages(),
                                messages.as_ref().clone(),
                                copy_scope,
                            ));
                            ui.set_conversation_focused(true);
                            recovery.publish(
                                &ui,
                                crate::timeline_recovery::Pane::Room,
                                presentation.has_undecryptable,
                                recovery_lease,
                                security,
                            );
                            // VIEWPORT LIFECYCLE: under the checks above.
                            crate::timeline_viewport::rows_published(
                                &ui,
                                &selected_account,
                                navigation_epoch,
                                generation,
                                &gate,
                            );
                        }
                    });
                    if queued.is_err() {
                        break;
                    }
                }
                if !matches!(selection.guard(updates.changed()).await, Some(Ok(()))) {
                    break;
                }
            }
        });
        *feed_slot = Some(handle);
        Ok(true)
    }

    /// Leave the focused conversation view `shown` (`None`: no view was
    /// installed any more when the user asked): end its feed and show the
    /// open room's latest messages again. A stale request — another
    /// selection of the account (A -> B -> A), another room, or a view shown
    /// since — changes nothing. Drafts and an open thread are kept.
    pub(crate) async fn return_to_live_for(
        self: &Arc<Self>,
        selection: AccountSelection,
        room_id: String,
        shown: Option<morpheus_matrix::FocusedView>,
        ui: Weak<AppWindow>,
    ) {
        let account_id = selection.account_id().to_owned();
        let navigation_epoch = {
            let _transition = self.account_transition.lock().await;
            let mut key_slot = self.feed_key.lock().await;
            let mut feed_slot = self.feed.lock().await;
            if self.require_active_account(&account_id).await.is_err()
                || self.selected_room.lock().await.as_deref() != Some(room_id.as_str())
                || !selection.is_current()
            {
                return;
            }
            let current = self.focused_view(&room_id);
            let still_shown = match (&shown, &current) {
                (Some(shown), Some(view)) => view.same_view(shown),
                (None, None) => true,
                _ => false,
            };
            if !still_shown {
                return;
            }
            // No suspension from here to the end of the block.
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.recovery_notices
                .clear(crate::timeline_recovery::Pane::Room);
            self.drop_stale_focus();
            if let Some(task) = feed_slot.take() {
                task.abort();
            }
            *key_slot = None;
            // Any navigation after this check supersedes the reopen below.
            self.current_navigation_epoch()
        };
        self.open_room(selection, room_id, navigation_epoch, ui)
            .await;
    }

    pub async fn open_thread(
        self: &Arc<Self>,
        account_id: String,
        room_id: String,
        root_event_id: String,
        generation: u64,
        ui: Weak<AppWindow>,
    ) -> Result<(), MatrixCoreError> {
        let recovery_navigation = self.current_navigation_epoch();
        if self.thread_generation.load(Ordering::Acquire) != generation {
            return Ok(());
        }
        let selection = self
            .manager
            .account_selection(&account_id)
            .ok_or(MatrixCoreError::SelectedAccountChanged)?;
        {
            // A reveal still deciding has committed its room; it gives the
            // room back before it releases the barrier, so check after it.
            let _settled = self.snapshot_barrier.read().await;
            self.require_selected_room(&room_id).await?;
        }
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
        let mut updates = feed.subscribe_presentation();
        let current_generation = self.thread_generation.clone();
        let is_live = self.is_live.clone();
        // VIEWPORT LIFECYCLE: read-only, for the publication's security generation.
        let viewport_gate = self.security_gate.clone();
        let recovery = self.recovery_notices.clone();
        let recovery_lease = self.recovery_lease(
            selection.clone(),
            room_id.clone(),
            Some(root_event_id.clone()),
            false,
            generation,
            recovery_navigation,
        );
        let handle = tokio::spawn(async move {
            let _feed = feed;
            let delivery = Arc::new(crate::timeline_delivery::Latest::new());
            loop {
                let presentation = updates.borrow_and_update().clone();
                #[cfg(test)]
                let observed = presentation.messages.clone();
                let security = crate::timeline_viewport::security_generation(&viewport_gate);
                if delivery.submit((presentation, security)) {
                    let ui_for_update = ui.clone();
                    let current_generation = current_generation.clone();
                    let is_live = is_live.clone();
                    let recovery = recovery.clone();
                    let recovery_lease = recovery_lease.clone();
                    let gate = viewport_gate.clone();
                    let account_id = account_id.clone();
                    let room_id = room_id.clone();
                    let root_event_id = root_event_id.clone();
                    let pending = delivery.clone();
                    let selected_account = selection.clone();
                    if ui_for_update
                        .upgrade_in_event_loop(move |ui| {
                            let Some((presentation, security)) = pending.take() else {
                                return;
                            };
                            let messages = presentation.messages;
                            let copy_scope = crate::message_copy::capture_publication(
                                &ui,
                                &selected_account,
                                true,
                            );
                            if selected_account.is_current()
                                && is_live.load(Ordering::Acquire)
                                && current_generation.load(Ordering::Acquire) == generation
                                && ui.get_active_account_local_id().as_str() == account_id
                                && ui.get_active_room_id().as_str() == room_id
                                && ui.get_thread_root_id().as_str() == root_event_id
                            {
                                message_search::show_thread_root(
                                    &ui,
                                    &messages,
                                    &root_event_id,
                                    copy_scope.clone(),
                                );
                                let replies = messages
                                    .iter()
                                    .filter(|message| {
                                        message.event_id.as_deref() != Some(root_event_id.as_str())
                                    })
                                    .cloned()
                                    .collect();
                                ui.set_thread_messages(crate::timeline_model::update_scoped(
                                    ui.get_thread_messages(),
                                    replies,
                                    copy_scope,
                                ));
                                recovery.publish(
                                    &ui,
                                    crate::timeline_recovery::Pane::Thread,
                                    presentation.has_undecryptable,
                                    recovery_lease,
                                    security,
                                );
                                // VIEWPORT LIFECYCLE: under the checks above.
                                crate::timeline_viewport::replies_published(
                                    &ui,
                                    &selected_account,
                                    generation,
                                    &gate,
                                );
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
        let generation = self.thread_generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.recovery_notices
            .clear(crate::timeline_recovery::Pane::Thread);
        generation
    }

    pub fn thread_request_is_current(&self, generation: u64) -> bool {
        self.thread_generation.load(Ordering::Acquire) == generation
    }

    /// VIEWPORT LIFECYCLE (timeline_viewport): the thread request generation
    /// a reply's seek is bound to. Read only; never advanced from there.
    pub(crate) fn thread_request_generation(&self) -> u64 {
        self.thread_generation.load(Ordering::Acquire)
    }

    /// VIEWPORT LIFECYCLE (timeline_viewport): the feed generation a
    /// conversation seek is bound to (a new live feed, focused view or Return
    /// to live advances it). Read only; never advanced from there.
    pub(crate) fn feed_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
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
        self.require_media_context(catalog.account_id(), catalog.room_id())
            .await?;
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

    /// Whether this exact selection is still the live, active account.
    async fn require_selection(&self, selection: &AccountSelection) -> Result<(), MatrixCoreError> {
        if !self.is_live.load(Ordering::Acquire)
            || !selection.is_current()
            || self
                .manager
                .active_account()
                .await
                .as_ref()
                .map(|account| account.id.as_str())
                != Some(selection.account_id())
            || !selection.is_current()
        {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        Ok(())
    }

    /// A transfer-center download, bound to one exact account selection
    /// rather than the room on screen: moving between that account's rooms
    /// does not stop it. The core resolves the event in that account's room
    /// only, so another account's media is never reached.
    pub async fn download_attachment_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        event_id: &str,
    ) -> Result<DownloadedAttachment, MatrixCoreError> {
        self.require_selection(selection).await?;
        let downloaded = self
            .manager
            .download_attachment_for(selection.account_id(), room_id, event_id)
            .await?;
        self.require_selection(selection).await?;
        Ok(downloaded)
    }

    /// Commit a transfer-center download while this exact account selection
    /// stays current. Account switching waits for the bounded local write.
    /// Publication refuses existing destinations, including entries that
    /// appeared or changed after the Save dialog closed.
    pub async fn save_download_selected(
        &self,
        selection: &AccountSelection,
        path: PathBuf,
        bytes: Vec<u8>,
        cancelled: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<Option<PathBuf>, MatrixCoreError> {
        let _transition = self.account_transition.lock().await;
        self.require_selection(selection).await?;
        // Cancellation stays effective while waiting for the guard. Once
        // submitted, the real write is awaited and its outcome reported.
        let Some(write) = begin_download_save(path, bytes, cancelled) else {
            return Ok(None);
        };
        write
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?
            .map(Some)
            .map_err(MatrixCoreError::Io)
    }

    /// Commit decrypted bytes only while this exact account and room remain
    /// selected. Navigation and account switching wait for the bounded local
    /// write, preventing cross-account saves after an asynchronous download.
    pub async fn save_download_to(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        path: PathBuf,
        bytes: Vec<u8>,
        cancelled: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<Option<PathBuf>, MatrixCoreError> {
        let _transition = self.account_transition.lock().await;
        let selected = self.selected_room.lock().await;
        if !self.is_live.load(Ordering::Acquire) || selected.as_deref() != Some(room_id) {
            return Err(MatrixCoreError::SelectedRoomChanged);
        }
        if !selection.is_current()
            || self
                .manager
                .active_account()
                .await
                .as_ref()
                .map(|account| account.id.as_str())
                != Some(selection.account_id())
            || !selection.is_current()
        {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        // Cancellation remains effective while waiting for every commit guard.
        // Once submitted, await the real write; never report a cancelled save
        // after it has entered the noninterruptible commit.
        let Some(write) = begin_download_save(path, bytes, cancelled) else {
            return Ok(None);
        };
        write
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?
            .map(Some)
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
        // A focused view's rows may be missing from the live timeline.
        if let Some(view) = self.focused_view(&room_id) {
            return self
                .manager
                .toggle_focused_reaction_for(&account_id, &view, &event_id, &reaction)
                .await;
        }
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
        crate::kept_replies::forget_all();
    } else if selection_changed {
        // A reply being typed in the room being left is kept for its return.
        crate::kept_replies::leaving_room(ui);
    }
    if selection_changed {
        crate::message_copy::retire(ui);
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
        // Pinned rows, flags and filter text belong to the previous room.
        ui.invoke_reset_pinned_messages();
        // So are the thread browser's decrypted roots.
        crate::thread_browser::clear(ui);
        // So do the media browser's rows, search and load.
        crate::room_media_browser::reset_window(ui);
        // So do the unread divider, jump and mark state.
        crate::unread_navigation::reset_window(ui);
        // So do typing names, Seen-by rows and read receipt state.
        crate::room_activity::clear(ui);
        // So do its members' identity-change warnings and their details.
        crate::identity_warnings::reset(ui);
        // So do the room's roles, staged permission changes and review.
        crate::room_permissions::reset_window(ui);
        crate::reset_attachment_view(ui);
        // All-room results stay with their account across rooms, which is
        // how opening a hit in another room keeps the list it came from.
        crate::message_search::room_changed(ui, account_changed);
        // A focused view and a jump belong to the room that was left.
        ui.set_conversation_focused(false);
        ui.set_timeline_jump_event_id("".into());
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
    let invitations = view
        .rooms
        .iter()
        .filter(|room| room.membership == RoomMembership::Invited)
        .map(|room| room_item(room, false, false))
        .collect::<Vec<_>>();
    // Home shows the first few as cards; the invitations center lists all.
    ui.set_home_invitations(ModelRc::new(VecModel::from(
        invitations
            .iter()
            .take(crate::invitations::HOME_CARDS)
            .cloned()
            .collect::<Vec<_>>(),
    )));
    ui.set_invitations(ModelRc::new(VecModel::from(invitations)));
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
    crate::room_organization::remember(&view.active.id, &view.rooms);
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
    // The account menu above ran before this snapshot made the session
    // live, so its label read was skipped on the first publication. Run it
    // now; the once-per-selection guard keeps it to one read.
    crate::account_identity::refresh_active_label(ui, false);
    if account_changed && ui.get_page() == 1 {
        ui.invoke_refresh_security();
    }
    if close_sign_in {
        ui.set_login_loading(false);
        ui.set_login_error("".into());
        ui.set_show_sign_in(false);
    }
    if account_changed {
        // A conversation target that waited for this account opens now.
        crate::message_search::account_shown(ui);
    } else if selection_changed {
        crate::kept_replies::entered_room(ui);
    }
}

fn clear_account_context(ui: &AppWindow, security_gate: &crate::security::SecurityGate) {
    crate::message_copy::retire(ui);
    crate::security::clear_ui(ui, security_gate);
    // Calls cannot outlive their account, so its call authority is already
    // revoked here. Its call tiles, pictures and roster must not wait for
    // the asynchronous stage presenter.
    crate::call_stage::clear_private_presentation(ui);
    // Transfer filenames and room labels are private to this account. Do
    // not wait for a changed-property callback or a queued projection.
    ui.set_transfer_items(ModelRc::new(VecModel::<crate::TransferItem>::default()));
    ui.set_transfer_summary("".into());
    ui.set_transfer_active_count(0);
    ui.set_transfer_focus_pending(false);
    // Poll drafts, pending votes and the create dialog end with the account.
    crate::polls::clear_presentation(ui);
    // The public-profile editor's drafts, previews and errors end with it too.
    crate::account_identity::clear_presentation(ui);
    // Directory rows, chosen person and notices end with the account.
    crate::people_directory::clear_presentation(ui);
    // Member details and the ignore list belong to this account.
    crate::member_privacy::clear_presentation(ui);
    // Decrypted pinned rows are account-private and never wait for a task.
    ui.invoke_reset_pinned_messages();
    // As are the thread browser's decrypted roots.
    crate::thread_browser::clear(ui);
    // So are the media browser's rows.
    crate::room_media_browser::reset_window(ui);
    // So is the unread divider, jump and mark state.
    crate::unread_navigation::reset_window(ui);
    // So are the room's roles and staged permission changes.
    crate::room_permissions::reset_window(ui);
    // So is who is in this account's voice rooms.
    #[cfg(feature = "native-calls")]
    crate::voice_rosters::clear(ui);
    // Typing names and read receipts are account-private too.
    crate::room_activity::clear(ui);
    // So are identity-change warnings and the members they name.
    crate::identity_warnings::reset(ui);
    ui.set_room_actions_open(false);
    // Typed names, the read context and any outcome of a creation end with
    // the account; a late result of its attempt is dropped (room_creation).
    crate::room_creation::clear(ui);
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
    crate::space_explorer::clear(ui);
    // Room names and any reviewed change of a Space being managed.
    crate::space_management::clear(ui);
    crate::room_addresses::clear(ui);
    // A link's read and join are withdrawn before its card is hidden.
    crate::matrix_links::clear(ui);
    crate::withdraw_call_bar(ui);
    // The Leave dialog and its captured account are private to this context.
    crate::room_leave::clear_presentation(ui);
    ui.set_invitations(ModelRc::new(VecModel::<RoomItem>::default()));
    ui.set_home_invitations(ModelRc::new(VecModel::<RoomItem>::default()));
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
    crate::account_identity::retain_confirmed(
        &accounts
            .iter()
            .map(|account| account.id.as_str())
            .collect::<Vec<_>>(),
    );
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
                avatar: item.avatar.clone(),
                has_avatar: item.has_avatar,
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
    ui.set_active_account_avatar(account.avatar.clone());
    ui.set_active_account_has_avatar(account.has_avatar);
    // Once per account selection: read the visible account's own public
    // profile for this label (never an inactive account's).
    crate::account_identity::refresh_active_label(ui, false);
}

fn account_item(account: &AccountInfo, selected: bool) -> AccountItem {
    // A display name this session confirmed with the server, else the
    // user ID's localpart. Never a locally stored identity.
    let name = crate::account_identity::confirmed_name(&account.id)
        .unwrap_or_else(|| display_user(&account.user_id));
    let account_initials = initials(&name);
    // An avatar this session already read for this exact local account, from
    // memory; nothing is fetched here and inactive accounts are never read.
    let avatar = crate::account_identity::avatar_image(&account.id);
    AccountItem {
        name: name.into(),
        handle: account.user_id.as_str().into(),
        initials: account_initials.into(),
        status: if selected { "Connected" } else { "Paused" }.into(),
        accent: accent(&account.id),
        has_avatar: avatar.is_some(),
        avatar: avatar.unwrap_or_default(),
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
        favourite: room.favourite,
        low_priority: room.low_priority,
        marked_unread: room.marked_unread,
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
        copy_token: Default::default(),
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
        attachment_is_audio: info.attachment_is_audio,
        reaction: info.reaction_summary.as_str().into(),
        poll: crate::polls::poll_card(info),
        accent: accent(&info.sender),
        pinned: crate::pinned_messages::row_pinned(info.event_id.as_deref()),
    }
}

pub(crate) fn display_user(id: &str) -> String {
    id.trim_start_matches('@')
        .split(':')
        .next()
        .unwrap_or(id)
        .to_owned()
}

pub(crate) fn initials(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_alphanumeric())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect()
}

pub(crate) fn accent(value: &str) -> Color {
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

/// Publish the final coalesced live snapshot only after the caller's exact
/// selection, feed, navigation and visible-room checks. This notification is
/// not an authority token: even a same-length replacement with the same newest
/// event must place the unread marker again. Focused feeds and divider writes
/// do not call it.
#[cfg(test)]
pub(crate) fn publish_live_messages(ui: &AppWindow, messages: Vec<Arc<MessageInfo>>) {
    publish_live_messages_scoped(ui, messages, None);
}

fn publish_live_messages_scoped(
    ui: &AppWindow,
    messages: Vec<Arc<MessageInfo>>,
    scope: Option<std::rc::Rc<crate::message_copy::CopyScope>>,
) {
    ui.set_messages(crate::timeline_model::update_scoped(
        ui.get_messages(),
        messages,
        scope,
    ));
    // Live rows replace any focused view, and with it that view's history
    // paging state. Notify only after both the model and that state are final.
    if ui.get_conversation_focused() {
        ui.set_conversation_focused(false);
        message_search::reset_history(ui);
    }
    ui.set_unread_message_publication(ui.get_unread_message_publication().wrapping_add(1));
}

fn snapshot_closes_sign_in(was_live: bool, account_changed: bool) -> bool {
    !was_live || account_changed
}

#[cfg(test)]
mod member_tests {
    use super::*;

    /// A member load's loading flag, members and error apply only while it
    /// is still the latest load for the account and room shown: a newer
    /// same-account, same-room load (a forced refresh, A -> B -> A) leaves
    /// the older one nothing to change. The 30 s coalescing relies only on a
    /// request that is still the latest, for the same account and room.
    /// Owner-free here; owners need a real selection (stale_observer_tests).
    #[test]
    fn a_member_load_publishes_only_while_it_is_the_latest_for_what_is_shown() {
        let is_live = Arc::new(AtomicBool::new(true));
        let member_epoch = Arc::new(AtomicU64::new(7));
        let navigation = Arc::new(AtomicU64::new(3));
        let load = |epoch| MemberPublication {
            epoch,
            account_id: "account".into(),
            room_id: "!room:example.org".into(),
            owner: None,
            is_live: is_live.clone(),
            member_epoch: member_epoch.clone(),
            navigation_epoch: navigation.clone(),
        };
        let older = load(7);
        assert!(older.admits("account", "!room:example.org"));
        // A newer load of the same account and room claims.
        let newer = load(member_epoch.fetch_add(1, Ordering::AcqRel) + 1);
        assert!(!older.admits("account", "!room:example.org"));
        assert!(newer.admits("account", "!room:example.org"));
        assert!(!newer.admits("other", "!room:example.org"));
        assert!(!newer.admits("account", "!other:example.org"));
        is_live.store(false, Ordering::Release);
        assert!(!newer.admits("account", "!room:example.org"));
        is_live.store(true, Ordering::Release);

        let request = |started, epoch| MemberRequest {
            account_id: "account".into(),
            room_id: "!room:example.org".into(),
            started,
            epoch,
            owner: None,
        };
        let now = Instant::now();
        assert!(request(now, 8).serves("account", "!room:example.org", 8, 3));
        // A navigation or a newer load since advanced the epoch.
        assert!(!request(now, 8).serves("account", "!room:example.org", 9, 4));
        assert!(!request(now, 8).serves("account", "!other:example.org", 8, 3));
        assert!(!request(now, 8).serves("other", "!room:example.org", 8, 3));
        if let Some(earlier) = now.checked_sub(Duration::from_secs(31)) {
            assert!(!request(earlier, 8).serves("account", "!room:example.org", 8, 3));
        }
    }

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
            attachment_is_audio: false,
            poll: None,
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

    /// A join's receipt owns what is shown only while nothing navigated since
    /// its own open and no security step began since its click; the join's
    /// own withdrawal once it returned (JoinNavigationWait) is no part of it.
    #[tokio::test]
    async fn a_join_receipt_owns_the_screen_only_until_the_next_navigation() {
        let (directory, live) = empty_profile();
        live.is_live.store(true, Ordering::Release);
        let asked = live.join_asked();
        assert_eq!(asked.owned_open(&live), None, "no open, no receipt");
        // Its own open took the next navigation and showed the room.
        let opened = live.invalidate_navigation();
        asked.record_opened(opened, "!room:example.org".into());
        let receipt = JoinOpened {
            navigation: opened,
            room_id: "!room:example.org".into(),
        };
        assert_eq!(asked.owned_open(&live), Some(receipt));
        asked.withdraw();
        assert!(asked.owned_open(&live).is_some());
        // A newer activation, of the same room too, ends it for good.
        let newer = live.invalidate_navigation();
        assert_eq!(asked.owned_open(&live), None);
        assert!(live.navigation_is_current(newer));
        // So does a security step begun and ended since the click.
        let other = live.join_asked();
        let taken = live.invalidate_navigation();
        other.record_opened(taken, "!room:example.org".into());
        assert!(other.owned_open(&live).is_some());
        drop(live.security_gate.try_account_access());
        assert_eq!(other.owned_open(&live), None);
        // And the session ending.
        let last = live.join_asked();
        let shown = live.invalidate_navigation();
        last.record_opened(shown, "!room:example.org".into());
        live.is_live.store(false, Ordering::Release);
        assert_eq!(last.owned_open(&live), None);
        close_profile(directory, live).await;
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

    /// An accepted invitation's late result never takes or undoes a newer
    /// navigation: its open claims the navigation it was chosen in only while
    /// that is still current, in one step, and commits only under that claim
    /// and its own authority, which a closed fence withdraws.
    #[tokio::test]
    async fn a_late_invitation_answer_never_takes_or_undoes_a_newer_navigation() {
        let (directory, live) = empty_profile();
        live.is_live.store(true, Ordering::Release);
        let (old_room, invited) = ("!old-room:example.org", "!invited:example.org");
        *live.selected_room.lock().await = Some(old_room.into());
        // Already stale before the answer's result is recorded: chosen on a
        // card, then another room chosen. The claim fails and changes no
        // epoch.
        let chosen = live.current_navigation_epoch();
        let newer = live.invalidate_navigation();
        let render = live.render_epoch.load(Ordering::Acquire);
        let members = live.member_epoch.load(Ordering::Acquire);
        assert_eq!(live.claim_navigation_from(chosen), None);
        assert!(live.navigation_is_current(newer));
        assert_eq!(live.render_epoch.load(Ordering::Acquire), render);
        assert_eq!(live.member_epoch.load(Ordering::Acquire), members);
        // The bookkeeping refresh after an answer takes no navigation.
        let _ = live.refresh(Weak::default()).await;
        assert!(live.navigation_is_current(newer));
        assert_eq!(live.selected_room.lock().await.as_deref(), Some(old_room));

        // Still current: the claim takes the next navigation. A room chosen
        // while its reveal is held at the selected-state commit wins.
        let claimed = live.claim_navigation_from(newer).expect("still current");
        assert!(live.navigation_is_current(claimed));
        let authority = NavigationAuthority::default();
        let held = live.selected_room.lock().await;
        let commit = {
            let (live, authority) = (live.clone(), authority.clone());
            tokio::spawn(async move {
                live.commit_reveal(invited, &[], true, claimed, &authority)
                    .await
            })
        };
        tokio::task::yield_now().await;
        let chosen_since = live.invalidate_navigation();
        drop(held);
        assert_eq!(commit.await.unwrap(), None);
        assert_eq!(live.selected_room.lock().await.as_deref(), Some(old_room));
        assert!(live.navigation_is_current(chosen_since));

        // A real security step begins and ends while the reveal is held at
        // its selected-state commit: the open's own conditions, captured at
        // its click, no longer hold, so the commit itself refuses, though no
        // navigation came since. No watcher and no withdrawal is involved.
        assert!(live.navigation_is_current(chosen_since));
        let request = live.open_request(live.security_gate.ringing_generation());
        let claimed = live
            .claim_navigation_from(request.navigation)
            .expect("still current");
        let authority = NavigationAuthority::holding(request.holds.clone());
        assert!(!authority.withdrawn());
        let held = live.selected_room.lock().await;
        let commit = {
            let (live, authority) = (live.clone(), authority.clone());
            tokio::spawn(async move {
                live.commit_reveal(invited, &[], true, claimed, &authority)
                    .await
            })
        };
        tokio::task::yield_now().await;
        drop(live.security_gate.try_account_access());
        drop(held);
        assert_eq!(commit.await.unwrap(), None);
        assert_eq!(live.selected_room.lock().await.as_deref(), Some(old_room));
        assert!(live.navigation_is_current(claimed));
        // Every later check of that authority, the final snapshot's
        // admission included (SnapshotGate asks `withdrawn`), refuses too.
        assert!(authority.withdrawn());

        // The joined-Space commit (commit_joined_space) asks the same
        // conditions under its selected-state locks; it needs a real
        // selection, so that case runs in
        // a_departed_selection_neither_reveals_nor_focuses_after_a_switch_back.
        // The session ending is one of those conditions too.
        let request = live.open_request(live.security_gate.ringing_generation());
        let authority = NavigationAuthority::holding(request.holds.clone());
        assert!(!authority.withdrawn());
        live.is_live.store(false, Ordering::Release);
        assert!(authority.withdrawn());
        close_profile(directory, live).await;
    }

    /// The invitation path's account transition wait is bounded, and gives
    /// the transition as soon as it is free.
    #[tokio::test]
    async fn the_invitation_transition_wait_is_bounded() {
        let (directory, live) = empty_profile();
        let held = live.account_transition.lock().await;
        let started = Instant::now();
        let waited = live.bounded_transition(Duration::from_millis(100)).await;
        assert!(waited.is_none());
        drop(waited);
        assert!(started.elapsed() >= Duration::from_millis(100));
        drop(held);
        let taken = live.bounded_transition(Duration::from_millis(100)).await;
        assert!(taken.is_some());
        drop(taken);
        close_profile(directory, live).await;
    }

    #[tokio::test]
    async fn a_withdrawn_search_navigation_held_at_its_commit_changes_nothing() {
        let (directory, live) = empty_profile();
        live.is_live.store(true, Ordering::Release);
        let (old_room, old_space) = ("!old-room:example.org", "!old-space:example.org");
        let (hit_room, hit_space) = ("!hit-room:example.org", "!hit-space:example.org");
        *live.selected_room.lock().await = Some(old_room.into());
        *live.selected_space.lock().await = Some(old_space.into());
        let spaces = vec![hit_space.to_owned()];
        let epoch = live.invalidate_navigation();
        let authority = NavigationAuthority::default();

        // Hold the reveal at its commit, withdraw it there, then release it.
        let held = live.selected_room.lock().await;
        let commit = {
            let (live, spaces, authority) = (live.clone(), spaces.clone(), authority.clone());
            tokio::spawn(async move {
                let commit = live.commit_reveal(hit_room, &spaces, false, epoch, &authority);
                commit.await
            })
        };
        tokio::task::yield_now().await;
        authority.withdraw();
        drop(held);
        assert_eq!(commit.await.unwrap(), None);
        assert_eq!(live.selected_room.lock().await.as_deref(), Some(old_room));
        assert_eq!(live.selected_space.lock().await.as_deref(), Some(old_space));

        // Withdrawing the old search navigation never revokes a newer one.
        let newer = live.invalidate_navigation();
        authority.withdraw();
        assert!(live.navigation_is_current(newer));

        // A held authority commits the room and its Space together; a
        // rollback before the room is shown restores both.
        let authority = NavigationAuthority::default();
        let commit = live.commit_reveal(hit_room, &spaces, false, newer, &authority);
        let previous = commit.await.expect("a current reveal commits");
        assert_eq!(live.selected_room.lock().await.as_deref(), Some(hit_room));
        assert_eq!(live.selected_space.lock().await.as_deref(), Some(hit_space));
        live.restore_reveal(hit_room, previous).await;
        assert_eq!(live.selected_room.lock().await.as_deref(), Some(old_room));
        assert_eq!(live.selected_space.lock().await.as_deref(), Some(old_space));
        close_profile(directory, live).await;
    }

    #[tokio::test]
    async fn no_refresh_samples_a_reveal_until_it_is_decided_or_given_back() {
        let (directory, live) = empty_profile();
        live.is_live.store(true, Ordering::Release);
        let (old_room, hit_room) = ("!old-room:example.org", "!hit-room:example.org");
        *live.selected_room.lock().await = Some(old_room.into());
        let epoch = live.invalidate_navigation();
        let authority = NavigationAuthority::default();

        // A reveal holds the barrier from before it reads the rooms: a refresh
        // started meanwhile (a sync update) waits before it samples or bumps
        // anything, so it can never show the committed room unguarded.
        let barrier = live.snapshot_barrier.write().await;
        let previous = live
            .commit_reveal(hit_room, &[], true, epoch, &authority)
            .await
            .expect("a current reveal commits");
        let render = live.render_epoch.load(Ordering::Acquire);
        let sampling = live.pauses.install("refresh.after_barrier");
        let refresh = {
            let live = live.clone();
            tokio::spawn(async move { live.refresh(Weak::default()).await })
        };
        let waited = tokio::time::timeout(Duration::from_millis(50), sampling.entered()).await;
        assert!(waited.is_err());
        assert_eq!(live.render_epoch.load(Ordering::Acquire), render);

        // Withdrawn before its snapshot: the reveal gives the room back while
        // it still holds the barrier, and only then releases it, so the
        // waiting refresh first passes the barrier with the old room selected.
        authority.withdraw();
        let release = live.pauses.install("reveal.before_release");
        let settle = live.settle_unshown_reveal(hit_room, previous, Some(barrier));
        let check = async {
            release.entered().await;
            assert_eq!(live.selected_room.lock().await.as_deref(), Some(old_room));
            let held = tokio::time::timeout(Duration::from_millis(50), sampling.entered()).await;
            assert!(held.is_err());
            release.proceed();
            sampling.entered().await;
            assert_eq!(live.render_epoch.load(Ordering::Acquire), render);
            sampling.proceed();
        };
        tokio::join!(settle, check);
        assert!(matches!(
            refresh.await.unwrap(),
            Err(MatrixCoreError::NoActiveAccount)
        ));
        assert!(live.render_epoch.load(Ordering::Acquire) > render);

        // Giving a reveal back never undoes a newer navigation's room.
        let authority = NavigationAuthority::default();
        let previous = live
            .commit_reveal(hit_room, &[], true, epoch, &authority)
            .await
            .expect("a current reveal commits");
        let newer_room = "!newer-room:example.org";
        *live.selected_room.lock().await = Some(newer_room.into());
        live.restore_reveal(hit_room, previous).await;
        assert_eq!(live.selected_room.lock().await.as_deref(), Some(newer_room));
        close_profile(directory, live).await;
    }

    #[test]
    fn a_focused_view_loaded_out_of_order_never_replaces_the_shown_one() {
        let generation = AtomicU64::new(7);
        let mut slot = FocusSlot::default();
        let view_a = NavigationAuthority::default();
        let view_b = NavigationAuthority::default();
        // A starts loading; B is opened before A finishes, which withdraws A.
        view_a.withdraw();
        let shown = slot.commit(&generation, &view_b, "B").expect("B commits");
        // A finishes after B: its commit is refused and B stays the paged view.
        assert_eq!(slot.commit(&generation, &view_a, "A"), None);
        assert_eq!(generation.load(Ordering::Acquire), shown);
        assert_eq!(slot.view(shown), Some("B"));
        // Dropping a stale handle or a stale generation cannot remove B.
        slot.drop_stale(shown);
        assert_eq!(slot.view(shown), Some("B"));
        // A newer feed (live, or another view) makes B stale: nothing is paged
        // through it and it is forgotten.
        let live = generation.fetch_add(1, Ordering::AcqRel) + 1;
        assert_eq!(slot.view(live), None);
        slot.drop_stale(live);
        assert!(slot.shown.is_none());
    }

    /// A withdrawal wakes a reveal where it waits before its commit or
    /// before its snapshot is decided (an unbounded read of the rooms, the
    /// barrier, the transition lock), so a timed-out reveal gives those up
    /// at once instead of holding navigation.
    #[tokio::test]
    async fn a_withdrawal_stops_a_reveal_waiting_on_a_read_at_once() {
        let authority = NavigationAuthority::default();
        let waiting = {
            let authority = authority.clone();
            tokio::spawn(
                async move { unless_withdrawn(&authority, std::future::pending::<()>()).await },
            )
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        authority.withdraw();
        let woke = tokio::time::timeout(Duration::from_secs(5), waiting).await;
        assert_eq!(woke.expect("woken by the withdrawal").unwrap(), None);
        // Already withdrawn: no wait at all, even for work that is ready.
        assert_eq!(unless_withdrawn(&authority, async { 7 }).await, None);
        let current = NavigationAuthority::default();
        assert_eq!(unless_withdrawn(&current, async { 7 }).await, Some(7));
        // The barrier held by a stalled reveal is released with it.
        let (directory, live) = empty_profile();
        let held = live.snapshot_barrier.write().await;
        let blocked = NavigationAuthority::default();
        let barrier = {
            let (live, blocked) = (live.clone(), blocked.clone());
            tokio::spawn(async move {
                unless_withdrawn(&blocked, live.snapshot_barrier.write())
                    .await
                    .is_some()
            })
        };
        tokio::task::yield_now().await;
        blocked.withdraw();
        let gave_up = tokio::time::timeout(Duration::from_secs(5), barrier).await;
        assert!(!gave_up.expect("woken by the withdrawal").unwrap());
        drop(held);
        close_profile(directory, live).await;
    }

    /// Whichever thread gets there first, the UI's claim of an owned
    /// snapshot (SnapshotGate::admits) and a withdrawal are one decision on
    /// the authority's one lock: the withdrawal reports a claim exactly when
    /// the claim succeeded, and after a withdrawal nothing is claimed, so a
    /// requester never reads a claimed snapshot as refused. No lock is held
    /// after either returns (the apply runs outside it). Commits keep their
    /// rule, and an authority whose own conditions failed (`holding`) claims
    /// nothing and wakes every wait at once.
    #[test]
    fn a_claim_and_a_withdrawal_are_one_decision_on_any_thread() {
        for _ in 0..500 {
            let authority = NavigationAuthority::default();
            let start = Arc::new(std::sync::Barrier::new(2));
            let claiming = {
                let (authority, start) = (authority.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    authority.claim()
                })
            };
            start.wait();
            let saw_claim = authority.withdraw();
            let claimed = claiming.join().unwrap();
            assert_eq!(claimed, saw_claim, "a claim and a withdrawal disagreed");
            assert_eq!(authority.admitted(), claimed);
            assert!(authority.withdrawn());
            assert!(!authority.claim(), "claimed after the withdrawal");
        }
        // Commits keep their rule: refused once withdrawn, claimed or not.
        let generation = AtomicU64::new(0);
        let mut slot = FocusSlot::default();
        let authority = NavigationAuthority::default();
        assert!(authority.claim());
        assert!(slot.commit(&generation, &authority, "A").is_some());
        assert!(authority.withdraw());
        assert_eq!(slot.commit(&generation, &authority, "B"), None);
        // Its own conditions failing ends it the same way, with no withdraw
        // call: nothing is claimed, and a wait before a commit returns at once.
        let holds = Arc::new(AtomicBool::new(true));
        let held = {
            let holds = holds.clone();
            NavigationAuthority::holding(Arc::new(move || holds.load(Ordering::Acquire)))
        };
        holds.store(false, Ordering::Release);
        assert!(!held.claim() && !held.admitted());
        assert!(held.withdrawn());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let ready = runtime.block_on(unless_withdrawn(&held, std::future::pending::<()>()));
        assert_eq!(ready, None, "a failed condition never keeps a wait");
        assert!(!held.withdraw(), "nothing was claimed");
    }

    /// An authority carrying its requester's click-time security generation
    /// (the shape of JoinAsked::follow_up_authority, whose selection part
    /// needs the opt-in fixture) refuses a held focused-view commit and a
    /// held room commit once a real security step began and ended since the
    /// click, and reports itself withdrawn to every later admission, with no
    /// withdraw call and no navigation change.
    #[tokio::test]
    async fn a_follow_up_held_at_its_commit_refuses_after_a_security_step_since_its_click() {
        fn clicked(live: &LiveUi) -> NavigationAuthority {
            let (gate, is_live) = (live.security_gate.clone(), live.is_live.clone());
            let security = gate.ringing_generation();
            NavigationAuthority::holding(Arc::new(move || {
                is_live.load(Ordering::Acquire) && gate.ringing_allowed(security)
            }))
        }
        let (directory, live) = empty_profile();
        live.is_live.store(true, Ordering::Release);
        let (old_room, hit_room) = ("!old-room:example.org", "!hit-room:example.org");
        *live.selected_room.lock().await = Some(old_room.into());
        let navigation = live.invalidate_navigation();
        let authority = clicked(&live);
        assert!(!authority.withdrawn());

        // The focused view loaded; a step begins and ends before its commit.
        let generation = AtomicU64::new(3);
        let mut slot = FocusSlot::default();
        let step = live.security_gate.try_account_access();
        assert!(step.is_some(), "an idle gate begins a step");
        assert!(authority.withdrawn(), "while the step runs");
        drop(step);
        assert!(authority.withdrawn(), "and after it ended");
        assert_eq!(slot.commit(&generation, &authority, "view"), None);
        assert_eq!(generation.load(Ordering::Acquire), 3);
        assert!(slot.shown.is_none());

        // A room commit held at its lock is refused the same way.
        let authority = clicked(&live);
        let held = live.selected_room.lock().await;
        let commit = {
            let (live, authority) = (live.clone(), authority.clone());
            tokio::spawn(async move {
                live.commit_reveal(hit_room, &[], true, navigation, &authority)
                    .await
            })
        };
        tokio::task::yield_now().await;
        drop(live.security_gate.try_account_access());
        drop(held);
        assert_eq!(commit.await.unwrap(), None);
        assert_eq!(live.selected_room.lock().await.as_deref(), Some(old_room));
        assert!(
            live.navigation_is_current(navigation),
            "no navigation was taken"
        );

        // Unchanged since its click, it commits; the session ending stops it.
        let authority = clicked(&live);
        let shown = slot.commit(&generation, &authority, "view");
        assert_eq!(shown, Some(4));
        live.is_live.store(false, Ordering::Release);
        assert!(authority.withdrawn());
        assert_eq!(slot.commit(&generation, &authority, "later"), None);
        assert_eq!(slot.view(4), Some("view"));
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
    // Additional fixed diagnostics for the one unresolved rollback-write case.
    // These are observations only; the twelve completion checks stay unchanged.
    macro_rules! write_revocation_boundary {
        ($boundary:ident) => {
            eprintln!(
                "\nMORPHEUS_APP_RECOVERY_BOUNDARY write_revocation {}",
                stringify!($boundary)
            );
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

    /// Logout may return promptly while its already-started native cleanup is
    /// still owned. Observe only that same operation; never retry the logout or
    /// infer erasure from a permit/count. A real vault/store failure still fails.
    async fn logout_fixture_account(
        manager: &AccountManager,
        id: &str,
    ) -> std::result::Result<(), MatrixCoreError> {
        match manager.logout_account(id).await {
            Ok(_) => {}
            Err(MatrixCoreError::AccountRemovalPending) => {
                eprintln!("MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=logout_pending");
                let observed = tokio::time::timeout(
                    Duration::from_secs(30),
                    manager.finish_pending_account_removal(id),
                )
                .await;
                let removal = match observed {
                    Ok(Ok(removal)) => removal,
                    Ok(Err(error)) => {
                        eprintln!(
                            "MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=existing_observation_error"
                        );
                        return Err(error);
                    }
                    Err(_) => {
                        eprintln!(
                            "MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=existing_observation_timeout"
                        );
                        return Err(MatrixCoreError::AccountRemovalPending);
                    }
                };
                eprintln!(
                    "MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=existing_finished complete={} credentials={} store={}",
                    u8::from(removal.complete),
                    u8::from(removal.credentials_removed),
                    u8::from(removal.store_removed),
                );
                if removal.account_id != id {
                    eprintln!(
                        "MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=wrong_account complete={} credentials={} store={}",
                        u8::from(removal.complete),
                        u8::from(removal.credentials_removed),
                        u8::from(removal.store_removed),
                    );
                    return Err(MatrixCoreError::AccountRemovalPending);
                }
                if !removal.complete || !removal.credentials_removed || !removal.store_removed {
                    return Err(MatrixCoreError::AccountRemovalPending);
                }
            }
            Err(error) => return Err(error),
        }
        if manager
            .accounts()
            .await
            .iter()
            .any(|account| account.id == id)
            || manager
                .app_password_status()
                .await
                .pending_removals
                .iter()
                .any(|pending| pending == id)
        {
            eprintln!("MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=registry_remaining");
            return Err(MatrixCoreError::AccountRemovalPending);
        }
        Ok(())
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

        logout_fixture_account(&manager, &a_account.id)
            .await
            .expect("Cannot remove first account");
        logout_fixture_account(&manager, &b_account.id)
            .await
            .expect("Cannot remove second account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
        recovery_check!(observer_revocation);
    }

    /// Work of an opening started under a departed selection (A -> B -> A)
    /// never reacquires ownership from its account ID: a late reveal or
    /// focus changes nothing, though the account is visible again under a
    /// renewed selection. A reveal snapshot's gate admits only the
    /// authority, selection and navigation epoch it was opened under.
    #[tokio::test]
    async fn a_departed_selection_neither_reveals_nor_focuses_after_a_switch_back() {
        let Some((alice, carol, a_room)) = fixture() else {
            eprintln!(
                "Skipped navigation regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
            );
            return;
        };
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
        live.is_live.store(true, Ordering::Release);
        let selection = manager
            .account_selection(&a_account.id)
            .expect("Selection missing");

        // Use the real marker authority captured before A -> B -> A. Both
        // destinations change only through its actual synchronous commit gate;
        // no test-owned predicate mirrors the selection or close checks.
        let marker = live
            .marker_scans
            .authority_for_test(&live, selection.clone());
        let (mut marker_cache, mut marker_presentation) = (0, 0);
        assert!(marker.is_current());
        assert_eq!(
            marker.with_current(|| {
                marker_cache = 1;
                marker_presentation = 1;
            }),
            Some(())
        );
        assert_eq!((marker_cache, marker_presentation), (1, 1));

        // Actual postjoin dispatch must stay withdrawable while the account
        // transition is held. It neither waits for that lock to be released
        // after Close nor invalidates the old room's feed while waiting.
        *live.selected_room.lock().await = Some(a_room.clone());
        for kind in [JoinedNavigation::Room, JoinedNavigation::Space] {
            let transition = live.account_transition.lock().await;
            let asked = live.join_asked();
            let navigation = live.current_navigation_epoch();
            let generation = live.generation.load(Ordering::Acquire);
            let opening = tokio::spawn(live.clone().open_joined(
                selection.clone(),
                "!not-opened:example.org".into(),
                kind,
                asked.clone(),
                Weak::default(),
            ));
            tokio::task::yield_now().await;
            asked.withdraw();
            let result = tokio::time::timeout(Duration::from_secs(2), opening)
                .await
                .expect("Close must wake a held postjoin transition")
                .unwrap();
            assert!(matches!(result, Ok(false)));
            assert_eq!(live.current_navigation_epoch(), navigation);
            assert_eq!(live.generation.load(Ordering::Acquire), generation);
            assert_eq!(
                live.selected_room.lock().await.as_deref(),
                Some(a_room.as_str())
            );
            assert!(live.selected_space.lock().await.is_none());
            drop(transition);
        }
        // A newer navigation requested while the join waits keeps its own
        // epoch. Releasing the transition cannot implicitly open the old join.
        let transition = live.account_transition.lock().await;
        let asked = live.join_asked();
        let opening = tokio::spawn(live.clone().open_joined(
            selection.clone(),
            "!old:example.org".into(),
            JoinedNavigation::Space,
            asked,
            Weak::default(),
        ));
        let newer = live.invalidate_navigation();
        drop(transition);
        assert!(matches!(opening.await.unwrap(), Ok(false)));
        assert_eq!(live.current_navigation_epoch(), newer);
        assert_eq!(
            live.selected_room.lock().await.as_deref(),
            Some(a_room.as_str())
        );
        // Withdrawal also wins after an individual selection lock was held,
        // at the synchronous Space commit boundary rather than just entry.
        let held_room = live.selected_room.lock().await;
        let authority = NavigationAuthority::default();
        let committing = tokio::spawn({
            let (live, selection, authority) = (live.clone(), selection.clone(), authority.clone());
            async move {
                live.commit_joined_space(&selection, "!new-space:example.org", newer, &authority)
                    .await
            }
        });
        tokio::task::yield_now().await;
        authority.withdraw();
        drop(held_room);
        assert!(committing.await.unwrap().is_none());
        assert!(live.selected_space.lock().await.is_none());
        assert_eq!(
            live.selected_room.lock().await.as_deref(),
            Some(a_room.as_str())
        );

        let authority = NavigationAuthority::default();
        let gate = SnapshotGate {
            authority: authority.clone(),
            selection: selection.clone(),
            navigation: live.invalidate_navigation(),
        };
        assert!(gate.admits(&live.navigation_epoch));
        live.invalidate_navigation();
        assert!(!gate.admits(&live.navigation_epoch));
        let gate = SnapshotGate {
            navigation: live.current_navigation_epoch(),
            ..gate
        };
        assert!(gate.admits(&live.navigation_epoch));
        authority.withdraw();
        // The admission it claimed is reported to the withdrawal (never read
        // as refused; a repeated withdrawal reports the same one decision),
        // which then ends every later admission.
        assert!(authority.withdraw(), "a claimed admission reads as claimed");
        assert!(!gate.admits(&live.navigation_epoch));
        // An accepted invitation's own open: a real security step begun and
        // ended since its click, and its snapshot is not admitted, though
        // nothing navigated.
        let request = live.open_request(live.security_gate.ringing_generation());
        let fenced = SnapshotGate {
            authority: NavigationAuthority::holding(request.holds.clone()),
            selection: selection.clone(),
            navigation: live.current_navigation_epoch(),
        };
        assert!(fenced.admits(&live.navigation_epoch));
        drop(live.security_gate.try_account_access());
        assert!(!fenced.admits(&live.navigation_epoch));
        // An accepted Space invitation's own open asks the same conditions
        // under the joined-Space commit's selected-state locks: a real
        // security step begun and ended while it is held there refuses it,
        // though no navigation came since.
        let space = "!invited-space:example.org";
        let request = live.open_request(live.security_gate.ringing_generation());
        let claimed = live
            .claim_navigation_from(request.navigation)
            .expect("still current");
        let authority = NavigationAuthority::holding(request.holds.clone());
        let held = live.selected_space.lock().await;
        let commit = {
            let (live, selection, authority) = (live.clone(), selection.clone(), authority.clone());
            tokio::spawn(async move {
                live.commit_joined_space(&selection, space, claimed, &authority)
                    .await
            })
        };
        tokio::task::yield_now().await;
        drop(live.security_gate.try_account_access());
        drop(held);
        assert!(commit.await.unwrap().is_none());
        assert!(live.selected_space.lock().await.is_none());
        assert_eq!(
            live.selected_room.lock().await.as_deref(),
            Some(a_room.as_str())
        );
        assert!(live.navigation_is_current(claimed));
        assert!(authority.withdrawn());
        // Unchanged conditions: it selects the Space with no room in one
        // step. The production give-back is reveal_joined_space_locked's;
        // the previous selection is put back here for the checks below.
        let request = live.open_request(live.security_gate.ringing_generation());
        let claimed = live
            .claim_navigation_from(request.navigation)
            .expect("still current");
        let authority = NavigationAuthority::holding(request.holds.clone());
        let previous = live
            .commit_joined_space(&selection, space, claimed, &authority)
            .await
            .expect("an unchanged owner commits");
        assert_eq!(live.selected_space.lock().await.as_deref(), Some(space));
        assert!(live.selected_room.lock().await.is_none());
        *live.selected_space.lock().await = previous.0;
        *live.selected_room.lock().await = previous.1;
        // INV-OPEN-01/02: an accepted invitation's open owned from its click
        // by this selection. A security step begun and ended since the click
        // leaves its one authority withdrawn: open_answered claims no
        // navigation and shows nothing.
        let navigation = live.current_navigation_epoch();
        let request = live.open_request(live.security_gate.ringing_generation());
        let owned = request.owned(&selection);
        drop(live.security_gate.try_account_access());
        assert!(owned.authority().withdrawn());
        live.open_answered(&selection, &a_room, false, Some(owned), Weak::default())
            .await;
        assert_eq!(
            live.current_navigation_epoch(),
            navigation,
            "nothing claimed"
        );
        assert_eq!(
            live.selected_room.lock().await.as_deref(),
            Some(a_room.as_str())
        );
        // A fresh one commits under its own authority the way the owned
        // reveal does (commit_reveal): the joined room in Home (it is in no
        // Space), although another Space was selected, so that Space's filter
        // never makes the refresh pick another room.
        *live.selected_space.lock().await = Some("!another-space:example.org".into());
        let request = live.open_request(live.security_gate.ringing_generation());
        let owned = request.owned(&selection);
        let claimed = live
            .claim_navigation_from(owned.navigation)
            .expect("still current");
        let previous = live
            .commit_reveal(&a_room, &[], true, claimed, owned.authority())
            .await
            .expect("an unchanged owner commits");
        assert!(live.selected_space.lock().await.is_none());
        assert_eq!(
            live.selected_room.lock().await.as_deref(),
            Some(a_room.as_str())
        );
        live.restore_reveal(&a_room, previous).await;
        *live.selected_space.lock().await = None;

        // A -> B -> A: the first selection is revoked, switching back issues
        // a new one, and the old one never admits a snapshot again.
        let b_account = manager
            .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
            .await
            .expect("Second login failed");
        manager
            .switch_to(&a_account.id)
            .await
            .expect("Cannot switch back");
        let renewed = manager
            .account_selection(&a_account.id)
            .expect("Renewed selection missing");
        assert!(!selection.is_current() && renewed.is_current());
        assert!(!marker.is_current());
        assert_eq!(
            marker.with_current(|| {
                marker_cache = 9;
                marker_presentation = 9;
            }),
            None
        );
        assert_eq!((marker_cache, marker_presentation), (1, 1));
        let gate = SnapshotGate {
            authority: NavigationAuthority::default(),
            selection: selection.clone(),
            navigation: live.current_navigation_epoch(),
        };
        assert!(!gate.admits(&live.navigation_epoch));

        // A late reveal and focus under the departed selection, with a
        // current epoch and authority, change nothing and refresh nothing.
        *live.selected_room.lock().await = Some(a_room.clone());
        let epoch = live.invalidate_navigation();
        let generation = live.generation.load(Ordering::Acquire);
        let render = live.render_epoch.load(Ordering::Acquire);
        let revealed = live
            .reveal_room_for(
                selection.clone(),
                a_room.clone(),
                epoch,
                NavigationAuthority::default(),
                Weak::default(),
            )
            .await;
        assert!(matches!(revealed, Ok(false)));
        let focused = live
            .focus_event_for(
                selection,
                a_room.clone(),
                "$missing:example.org",
                epoch,
                NavigationAuthority::default(),
                Weak::default(),
            )
            .await;
        assert!(matches!(focused, Ok(false)));
        assert_eq!(
            live.selected_room.lock().await.as_deref(),
            Some(a_room.as_str())
        );
        assert!(live.selected_space.lock().await.is_none());
        assert_eq!(live.generation.load(Ordering::Acquire), generation);
        assert_eq!(live.render_epoch.load(Ordering::Acquire), render);
        assert!(live.feed_key.lock().await.is_none());
        assert!(live.focused_view(&a_room).is_none());

        // A fresh authority for the renewed epoch admits normally. Closing
        // only the actual marker owner then refuses it even though this real
        // account selection and the live/action context remain current.
        let current_marker = live.marker_scans.authority_for_test(&live, renewed.clone());
        assert!(current_marker.is_current());
        assert_eq!(
            current_marker.with_current(|| {
                marker_cache = 2;
                marker_presentation = 2;
            }),
            Some(())
        );
        live.marker_scans.close();
        assert!(renewed.is_current());
        assert!(live.is_live.load(Ordering::Acquire));
        assert!(!live.action_admission_closed());
        assert!(!current_marker.is_current());
        assert_eq!(
            current_marker.with_current(|| {
                marker_cache = 9;
                marker_presentation = 9;
            }),
            None
        );
        assert_eq!((marker_cache, marker_presentation), (2, 2));
        live.drain_marker_scans()
            .await
            .expect("The actual marker owner must finish");

        logout_fixture_account(&manager, &a_account.id)
            .await
            .expect("Cannot remove first account");
        logout_fixture_account(&manager, &b_account.id)
            .await
            .expect("Cannot remove second account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
        eprintln!("\nMORPHEUS_APP_MARKER_AUTHORITY_CHECK complete=true");
    }

    /// A created room opens only through its own owned reveal, and only for
    /// the click that asked for it (open_created_room):
    /// - withdrawn before it starts (its bound passed), it takes no
    ///   navigation and commits nothing;
    /// - after a newer navigation since its click, or a security step begun
    ///   and ended since, it takes no navigation and commits nothing: the
    ///   newer navigation keeps its epoch and room;
    /// - current, it takes its own navigation and commits the exact room, but
    ///   its snapshot is never admitted without a UI decision (no window
    ///   here): it is not opened, and the commit is given back before any
    ///   other refresh may sample it;
    /// - nothing is created: the room ID given is the room revealed.
    #[tokio::test]
    async fn a_created_room_opens_only_for_its_own_click_and_admitted_snapshot() {
        let Some((alice, _, a_room)) = fixture() else {
            eprintln!(
                "Skipped creation open regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
            );
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(directory.path().join("profile")).unwrap());
        let voices = Arc::new(VoiceSpaces::new(morpheus_config::ConfigStore::at(
            directory.path().join("config.toml"),
        )));
        let live = LiveUi::new(manager.clone(), Handle::current(), Weak::default(), voices);
        let account = manager
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("Login failed");
        wait_for_room(&manager, &a_room).await;
        live.is_live.store(true, Ordering::Release);
        let selection = manager
            .account_selection(&account.id)
            .expect("Selection missing");
        let created = CreatedRoom {
            room_id: a_room.clone(),
            kind: CreationKind::Room,
            name: "Created".into(),
            parent: None,
            suggested: false,
        };
        let previous = "!previous:fixture.invalid";
        *live.selected_room.lock().await = Some(previous.into());
        let open = |asked: CreationAsked, authority: NavigationAuthority| {
            let (admitted, admission) = tokio::sync::oneshot::channel();
            let opening = live.clone().open_created_room(
                selection.clone(),
                created.clone(),
                Placement::Home,
                asked,
                authority,
                admitted,
                Weak::default(),
            );
            (opening, admission)
        };

        // Withdrawn first: no navigation of its own, nothing committed.
        let asked = live.creation_asked();
        let withdrawn = live.creation_authority(asked, &selection);
        assert!(!withdrawn.withdraw(), "nothing was claimed");
        let (opening, admission) = open(asked, withdrawn);
        assert!(matches!(opening.await, Ok(false)));
        assert!(admission.await.is_err(), "admitted while withdrawn");
        assert!(
            live.navigation_is_current(asked.navigation),
            "no navigation taken"
        );
        assert_eq!(live.selected_room.lock().await.as_deref(), Some(previous));

        // A newer navigation since the click keeps its epoch and room.
        let asked = live.creation_asked();
        let newer = live.invalidate_navigation();
        let authority = live.creation_authority(asked, &selection);
        let (opening, admission) = open(asked, authority.clone());
        assert!(matches!(opening.await, Ok(false)));
        assert!(admission.await.is_err());
        assert!(
            live.navigation_is_current(newer),
            "the newer navigation stays current"
        );
        assert_eq!(live.selected_room.lock().await.as_deref(), Some(previous));
        assert!(!authority.withdraw(), "nothing was claimed");

        // A security step begun and ended since the click: the same.
        let asked = live.creation_asked();
        drop(live.security_gate.try_account_access());
        let authority = live.creation_authority(asked, &selection);
        assert!(authority.withdrawn(), "its own conditions failed");
        let (opening, admission) = open(asked, authority);
        assert!(matches!(opening.await, Ok(false)));
        assert!(admission.await.is_err());
        assert!(live.navigation_is_current(asked.navigation));
        assert_eq!(live.selected_room.lock().await.as_deref(), Some(previous));

        // Current: its own navigation and the exact commit, never admitted
        // without a window, given back while the reveal holds the barrier.
        let release = live.pauses.install("reveal.before_release");
        let asked = live.creation_asked();
        let authority = live.creation_authority(asked, &selection);
        let (opening, admission) = open(asked, authority);
        let opening = tokio::spawn(opening);
        release.entered().await;
        assert!(
            !live.navigation_is_current(asked.navigation),
            "it took its own"
        );
        assert_eq!(live.selected_room.lock().await.as_deref(), Some(previous));
        assert!(live.snapshot_barrier.try_read().is_err());
        release.proceed();
        let opened = opening.await.expect("the open task ends");
        assert!(
            !matches!(opened, Ok(true)),
            "opened without an admitted snapshot"
        );
        // The milestone agrees: no admission was ever told.
        assert!(admission.await.is_err());

        manager
            .logout_account(&account.id)
            .await
            .expect("Cannot remove the account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
    }

    /// An admitted snapshot's member load claims the member epoch only while
    /// that snapshot's selection and navigation are still current, checked
    /// at the claim, after its awaits. A stale load held there while a
    /// newer navigation's load claims installs nothing: the newer epoch and
    /// request stay, so the newer load is neither suppressed nor left
    /// loading. A forced refresh held there while its room stopped being
    /// the selected one claims nothing either.
    #[tokio::test]
    async fn a_stale_snapshots_member_load_never_claims_over_a_newer_one() {
        let Some((alice, _, a_room)) = fixture() else {
            eprintln!(
                "Skipped member claim regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
            );
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(directory.path().join("profile")).unwrap());
        let voices = Arc::new(VoiceSpaces::new(morpheus_config::ConfigStore::at(
            directory.path().join("config.toml"),
        )));
        let live = LiveUi::new(manager.clone(), Handle::current(), Weak::default(), voices);
        let account = manager
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("Login failed");
        wait_for_room(&manager, &a_room).await;
        live.is_live.store(true, Ordering::Release);
        let selection = manager
            .account_selection(&account.id)
            .expect("Selection missing");
        let newer_room = "!newer:fixture.invalid";
        let request = |live: Arc<LiveUi>| async move {
            let request = live.member_request.lock().await;
            request
                .as_ref()
                .map(|request| (request.room_id.clone(), request.epoch))
        };

        // A: the admitted snapshot's load passes its early checks and waits
        // at the claim.
        *live.selected_room.lock().await = Some(a_room.clone());
        let stale = live.invalidate_navigation();
        let held = live.pauses.install("members.before_claim");
        let loading = tokio::spawn({
            let (live, account, room) = (live.clone(), account.id.clone(), a_room.clone());
            let owner = MemberOwner {
                selection: selection.clone(),
                navigation_epoch: stale,
            };
            async move {
                live.refresh_members_owned(account, room, Weak::default(), false, owner)
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(30), held.entered())
            .await
            .unwrap();
        // B: a newer navigation selects another room and claims its load.
        live.invalidate_navigation();
        *live.selected_room.lock().await = Some(newer_room.into());
        live.refresh_members_for(account.id.clone(), newer_room.into(), Weak::default(), true)
            .await;
        let newer = live.member_epoch.load(Ordering::Acquire);
        assert_eq!(
            request(live.clone()).await,
            Some((newer_room.to_owned(), newer))
        );
        held.proceed();
        tokio::time::timeout(Duration::from_secs(30), loading)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            live.member_epoch.load(Ordering::Acquire),
            newer,
            "the stale load installed no epoch"
        );
        assert_eq!(
            request(live.clone()).await,
            Some((newer_room.to_owned(), newer)),
            "and left the newer load its request"
        );

        // A forced refresh whose room is no longer selected at its claim.
        *live.selected_room.lock().await = Some(a_room.clone());
        let held = live.pauses.install("members.before_claim");
        let refreshing = tokio::spawn({
            let (live, account, room) = (live.clone(), account.id.clone(), a_room.clone());
            async move {
                live.refresh_members_for(account, room, Weak::default(), true)
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(30), held.entered())
            .await
            .unwrap();
        *live.selected_room.lock().await = Some(newer_room.into());
        held.proceed();
        tokio::time::timeout(Duration::from_secs(30), refreshing)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(live.member_epoch.load(Ordering::Acquire), newer);
        assert_eq!(
            request(live.clone()).await,
            Some((newer_room.to_owned(), newer))
        );

        // Every publication of a claimed load is fenced by the same owner,
        // when it would apply. A same-room navigation ends the older view's
        // publications; the new view's own load still publishes.
        let publication = |epoch, owner| MemberPublication {
            epoch,
            account_id: account.id.clone(),
            room_id: newer_room.into(),
            owner,
            is_live: live.is_live.clone(),
            member_epoch: live.member_epoch.clone(),
            navigation_epoch: live.navigation_epoch.clone(),
        };
        let older = MemberOwner {
            selection: selection.clone(),
            navigation_epoch: live.current_navigation_epoch(),
        };
        let shown = publication(newer, Some(older.clone()));
        assert!(shown.admits(&account.id, newer_room));
        live.invalidate_navigation();
        assert!(
            !shown.admits(&account.id, newer_room),
            "the older view's load"
        );
        let current = MemberOwner {
            selection: selection.clone(),
            navigation_epoch: live.current_navigation_epoch(),
        };
        let epoch = live.member_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        assert!(
            publication(epoch, Some(current.clone())).admits(&account.id, newer_room),
            "the new view's own load"
        );
        // Coalescing relies on an earlier request only while its owner is
        // the current one: a matching owner's serves the view, an older
        // owner's never suppresses it, even under the same epoch.
        let navigation = live.current_navigation_epoch();
        let earlier = |owner| MemberRequest {
            account_id: account.id.clone(),
            room_id: newer_room.into(),
            started: Instant::now(),
            epoch,
            owner,
        };
        assert!(earlier(Some(current)).serves(&account.id, newer_room, epoch, navigation));
        assert!(!earlier(Some(older)).serves(&account.id, newer_room, epoch, navigation));

        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
    }

    /// Follow-ups that waited for a real production key or commit lock
    /// cannot retire the feed of a newer room. Exercise the actual
    /// after_snapshot, refusal reopen and open_room paths; the last loads its
    /// real SDK feed before the forced commit interleaving. A created room's
    /// open withdrawn after it took the transition lock, before its own
    /// navigation, drops that guard at once and takes no navigation.
    #[tokio::test]
    async fn stale_creation_follow_ups_keep_a_newer_rooms_feed_and_transition() {
        let Some((alice, _, a_room)) = fixture() else {
            eprintln!(
                "Skipped creation follow-up regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
            );
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(directory.path().join("profile")).unwrap());
        let voices = Arc::new(VoiceSpaces::new(morpheus_config::ConfigStore::at(
            directory.path().join("config.toml"),
        )));
        let live = LiveUi::new(manager.clone(), Handle::current(), Weak::default(), voices);
        let account = manager
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("Login failed");
        wait_for_room(&manager, &a_room).await;
        live.is_live.store(true, Ordering::Release);
        let selection = manager
            .account_selection(&account.id)
            .expect("Selection missing");
        let newer_room = "!newer:fixture.invalid";
        let newer_key = (account.id.clone(), newer_room.to_owned());
        // A running sentinel feed stands for the newer navigation's task.
        let (started, running) = tokio::sync::oneshot::channel();
        let sentinel = tokio::spawn(async move {
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
        running.await.unwrap();
        *live.feed.lock().await = Some(sentinel);

        // Admitted reveal: block the real key wait, then install a newer
        // navigation's key and room while that lock is still ours.
        *live.selected_room.lock().await = Some(a_room.clone());
        let epoch = live.invalidate_navigation();
        let mut key = live.feed_key.lock().await;
        let held = live.pauses.install("snapshot.before_feed_key");
        let mut after = tokio::spawn({
            let live = live.clone();
            let after = AfterSnapshot {
                account_id: account.id.clone(),
                room_id: Some(a_room.clone()),
                selection: Some(selection.clone()),
                navigation_epoch: epoch,
                marker_rooms: Vec::new(),
                owned: Some((
                    live.begin_thread_request(),
                    Some("Old room draft error".into()),
                )),
                retire_feed: false,
                recover: true,
            };
            async move { live.after_snapshot(after, Weak::default()).await }
        });
        tokio::time::timeout(Duration::from_secs(30), held.entered())
            .await
            .unwrap();
        held.proceed();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut after)
                .await
                .is_err()
        );
        live.invalidate_navigation();
        *live.selected_room.lock().await = Some(newer_room.into());
        *key = Some(newer_key.clone());
        drop(key);
        tokio::time::timeout(Duration::from_secs(30), after)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*live.feed_key.lock().await, Some(newer_key.clone()));
        assert!(
            live.feed
                .lock()
                .await
                .as_ref()
                .is_some_and(|task| !task.is_finished()),
            "the newer navigation's feed runs on"
        );

        // Refused reveal: the reopened feed's key mutation has the same
        // final fence after waiting for the held key.
        let epoch = live.invalidate_navigation();
        let mut key = live.feed_key.lock().await;
        let mut reopen = tokio::spawn({
            let (live, selection) = (live.clone(), selection.clone());
            async move {
                live.reopen_unrevealed(&selection, epoch, Weak::default())
                    .await
            }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut reopen)
                .await
                .is_err()
        );
        live.invalidate_navigation();
        *key = Some(newer_key.clone());
        drop(key);
        tokio::time::timeout(Duration::from_secs(30), reopen)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*live.feed_key.lock().await, Some(newer_key.clone()));

        // A stale room open must not even advance the feed generation after
        // its initial key wait: it would invalidate the newer running feed.
        *live.selected_room.lock().await = Some(a_room.clone());
        let epoch = live.invalidate_navigation();
        let held = live.pauses.install("feed.before_claim");
        let opening = tokio::spawn({
            let (live, selection, room) = (live.clone(), selection.clone(), a_room.clone());
            async move {
                live.open_room(selection, room, epoch, Weak::default())
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(30), held.entered())
            .await
            .unwrap();
        let mut key = live.feed_key.lock().await;
        held.proceed();
        live.invalidate_navigation();
        *live.selected_room.lock().await = Some(newer_room.into());
        let generation = live.generation.fetch_add(1, Ordering::AcqRel) + 1;
        *key = Some(newer_key.clone());
        drop(key);
        tokio::time::timeout(Duration::from_secs(30), opening)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(live.generation.load(Ordering::Acquire), generation);
        assert_eq!(*live.feed_key.lock().await, Some(newer_key.clone()));

        // An actual loaded SDK feed reaches the real final commit. Hold its
        // first commit lock, let a newer room, epoch and generation win, then
        // let it resume: the newer task is neither aborted nor replaced.
        *live.selected_room.lock().await = Some(a_room.clone());
        let epoch = live.invalidate_navigation();
        let held = live.pauses.install("feed.before_commit");
        let mut opening = tokio::spawn({
            let (live, selection, room) = (live.clone(), selection.clone(), a_room.clone());
            async move {
                live.open_room(selection, room, epoch, Weak::default())
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(30), held.entered())
            .await
            .unwrap();
        let mut key = live.feed_key.lock().await;
        held.proceed();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut opening)
                .await
                .is_err()
        );
        live.invalidate_navigation();
        *live.selected_room.lock().await = Some(newer_room.into());
        let generation = live.generation.fetch_add(1, Ordering::AcqRel) + 1;
        *key = Some(newer_key.clone());
        drop(key);
        tokio::time::timeout(Duration::from_secs(30), opening)
            .await
            .unwrap()
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(live.generation.load(Ordering::Acquire), generation);
        assert_eq!(*live.feed_key.lock().await, Some(newer_key.clone()));
        assert!(
            live.feed
                .lock()
                .await
                .as_ref()
                .is_some_and(|task| !task.is_finished())
        );

        // Withdrawn after taking the transition lock but before its own
        // navigation: the guard ends with its block, no navigation is taken
        // and nothing waits on the held key.
        let asked = live.creation_asked();
        let authority = live.creation_authority(asked, &selection);
        let held = live.pauses.install("creation.after_transition");
        let (admitted, admission) = tokio::sync::oneshot::channel();
        let key = live.feed_key.lock().await;
        let opening = tokio::spawn(live.clone().open_created_room(
            selection.clone(),
            CreatedRoom {
                room_id: a_room,
                kind: CreationKind::Room,
                name: "Created".into(),
                parent: None,
                suggested: false,
            },
            Placement::Home,
            asked,
            authority.clone(),
            admitted,
            Weak::default(),
        ));
        tokio::time::timeout(Duration::from_secs(30), held.entered())
            .await
            .unwrap();
        assert!(live.account_transition.try_lock().is_err());
        assert!(!authority.withdraw(), "nothing was claimed");
        held.proceed();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(30), opening)
                .await
                .unwrap()
                .unwrap(),
            Ok(false)
        ));
        assert!(admission.await.is_err());
        assert!(live.account_transition.try_lock().is_ok());
        // Only an ordinary refresh (the Space rescan) may take it, briefly.
        tokio::time::timeout(Duration::from_secs(30), async {
            while live.snapshot_barrier.try_write().is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("no barrier stays held");
        assert!(
            live.navigation_is_current(asked.navigation),
            "no navigation taken"
        );
        assert_eq!(*key, Some(newer_key));
        drop(key);
        live.is_live.store(false, Ordering::Release);
        live.invalidate_navigation();
        let task = live.feed.lock().await.take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        manager
            .logout_account(&account.id)
            .await
            .expect("Cannot remove the account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
    }

    /// A created room's open withdrawn while held at a real production wait
    /// gives everything up at once:
    /// - before its commit, at the known-Spaces lock of reveal_room_locked,
    ///   it commits nothing, and the reopen of the shown room's feed under
    ///   the navigation it took then waits without the transition lock;
    /// - after its commit, at its snapshot's draft load (snapshot.draft),
    ///   with the transition lock and the barrier its own, it gives the
    ///   commit back before releasing the barrier, and no lock stays held.
    /// The admission is never told for either.
    #[tokio::test]
    async fn a_withdrawn_created_room_open_gives_up_held_waits_and_its_locks() {
        let Some((alice, _, a_room)) = fixture() else {
            eprintln!(
                "Skipped creation wait regression; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
            );
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(directory.path().join("profile")).unwrap());
        let voices = Arc::new(VoiceSpaces::new(morpheus_config::ConfigStore::at(
            directory.path().join("config.toml"),
        )));
        let live = LiveUi::new(manager.clone(), Handle::current(), Weak::default(), voices);
        let account = manager
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("Login failed");
        wait_for_room(&manager, &a_room).await;
        live.is_live.store(true, Ordering::Release);
        let selection = manager
            .account_selection(&account.id)
            .expect("Selection missing");
        let created = CreatedRoom {
            room_id: a_room.clone(),
            kind: CreationKind::Room,
            name: "Created".into(),
            parent: None,
            suggested: false,
        };
        let previous = "!previous:fixture.invalid";
        let until = |held: fn(&LiveUi) -> bool, live: Arc<LiveUi>| async move {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !held(&live) {
                assert!(Instant::now() < deadline, "timed out waiting for the open");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let transition_free = |live: &LiveUi| live.account_transition.try_lock().is_ok();

        // Held before its commit.
        *live.selected_room.lock().await = Some(previous.into());
        let held = live.known_spaces.lock().await;
        let asked = live.creation_asked();
        let authority = live.creation_authority(asked, &selection);
        let (admitted, admission) = tokio::sync::oneshot::channel();
        let opening = tokio::spawn(live.clone().open_created_room(
            selection.clone(),
            created.clone(),
            Placement::Home,
            asked,
            authority.clone(),
            admitted,
            Weak::default(),
        ));
        // Inside, with the barrier written and the transition lock held.
        until(
            |live| live.snapshot_barrier.try_read().is_err(),
            live.clone(),
        )
        .await;
        assert!(!transition_free(&live));
        assert!(!authority.withdraw(), "nothing was claimed");
        // At once: the transition lock is free, while the reopen's refresh
        // still waits for the held lock.
        until(transition_free, live.clone()).await;
        assert_eq!(live.selected_room.lock().await.as_deref(), Some(previous));
        drop(held);
        let opened = opening.await.expect("the open task ends");
        assert!(matches!(opened, Ok(false)));
        assert!(admission.await.is_err(), "admitted while withdrawn");

        // Held after its commit, at the draft load of its snapshot.
        *live.selected_room.lock().await = Some(previous.into());
        let draft = live.pauses.install("snapshot.draft");
        let release = live.pauses.install("reveal.before_release");
        let asked = live.creation_asked();
        let authority = live.creation_authority(asked, &selection);
        let (admitted, admission) = tokio::sync::oneshot::channel();
        let opening = tokio::spawn(live.clone().open_created_room(
            selection.clone(),
            created,
            Placement::Home,
            asked,
            authority.clone(),
            admitted,
            Weak::default(),
        ));
        draft.entered().await;
        assert_eq!(
            live.selected_room.lock().await.as_deref(),
            Some(a_room.as_str())
        );
        assert!(!transition_free(&live));
        assert!(live.snapshot_barrier.try_read().is_err());
        assert!(!authority.withdraw(), "nothing was claimed");
        // Woken at the draft load: given back while the barrier is held.
        release.entered().await;
        assert_eq!(live.selected_room.lock().await.as_deref(), Some(previous));
        release.proceed();
        let opened = tokio::time::timeout(Duration::from_secs(30), opening)
            .await
            .expect("woken by the withdrawal")
            .expect("the open task ends");
        assert!(matches!(opened, Ok(false)));
        assert!(admission.await.is_err(), "admitted while withdrawn");
        until(transition_free, live.clone()).await;
        until(
            |live| live.snapshot_barrier.try_write().is_ok(),
            live.clone(),
        )
        .await;

        manager
            .logout_account(&account.id)
            .await
            .expect("Cannot remove the account");
        manager.stop().await;
        drop(manager);
        close_profile(directory, live).await;
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

    #[derive(Debug, PartialEq, Eq)]
    enum WriteRevocationPause {
        Reached,
        WorkerEnded,
        TimedOut,
    }

    // A worker that exits before the named pause must fail the test instead of
    // leaving its observer waiting forever. Never release the pause here: the
    // caller must still prove the original pre-write interleaving itself.
    async fn wait_for_write_revocation_pause(
        point: &PausePoint,
        rollback: &mut tokio::task::JoinHandle<()>,
    ) -> WriteRevocationPause {
        let reached = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::select! {
                biased;
                _ = point.entered() => true,
                _ = &mut *rollback => false,
            }
        })
        .await;
        match reached {
            Ok(true) => WriteRevocationPause::Reached,
            Ok(false) => WriteRevocationPause::WorkerEnded,
            Err(_) => {
                rollback.abort();
                let _ = (&mut *rollback).await;
                WriteRevocationPause::TimedOut
            }
        }
    }

    #[tokio::test]
    async fn rollback_pause_wait_rejects_an_early_ended_worker() {
        let pauses = TestPauses::default();
        let point = pauses.install("rollback.before_write");
        let mut rollback = tokio::spawn(async {});
        assert_eq!(
            wait_for_write_revocation_pause(&point, &mut rollback).await,
            WriteRevocationPause::WorkerEnded
        );
        assert!(rollback.is_finished());
    }

    #[tokio::test]
    async fn rollback_pause_wait_does_not_release_the_reached_write_boundary() {
        let pauses = TestPauses::default();
        let point = pauses.install("rollback.before_write");
        let mut rollback = tokio::spawn(async move {
            pauses.reach("rollback.before_write").await;
        });
        assert_eq!(
            wait_for_write_revocation_pause(&point, &mut rollback).await,
            WriteRevocationPause::Reached
        );
        assert!(!rollback.is_finished());
        point.proceed();
        rollback.await.unwrap();
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
        write_revocation_boundary!(setup);
        let (directory, manager, live, a_account, selection) = prepared_live(&alice, &a_room).await;
        let scope = SendScope {
            account: a_account.id.clone(),
            room: a_room.clone(),
            thread: None,
        };
        let text = unique_text("rollback-write");
        live.drafts.set(&a_account.id, &a_room, &text.body);
        let revision = live.drafts.revision(&a_account.id, &a_room).unwrap();
        write_revocation_boundary!(prepare_record);
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
        write_revocation_boundary!(rollback_pause);
        let mut rollback = tokio::spawn(async move {
            write_revocation_boundary!(rollback_writer_lock);
            let tx = rb_live.drafts.begin_write_transaction().await;
            write_revocation_boundary!(rollback_release);
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
        match wait_for_write_revocation_pause(&point, &mut rollback).await {
            WriteRevocationPause::Reached => {}
            WriteRevocationPause::WorkerEnded => {
                write_revocation_boundary!(rollback_ended_before_pause);
                panic!("the rollback ended before its pre-write pause");
            }
            WriteRevocationPause::TimedOut => {
                write_revocation_boundary!(rollback_pause_timeout);
                panic!("the rollback did not reach its pre-write pause");
            }
        }
        assert!(
            live.drafts
                .record_status(&a_account.id, &a_room, None, &nonce)
                .is_none(),
            "released and removed from the cache"
        );
        write_revocation_boundary!(row_before_switch);
        assert!(
            row_has_record(&manager, &a_account.id, &a_room, None, &nonce).await,
            "not yet written"
        );
        write_revocation_boundary!(switch_account);
        let b_account = manager
            .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
            .await
            .expect("Second login failed");
        assert!(!selection.is_current());
        point.proceed();
        write_revocation_boundary!(rollback_join);
        tokio::time::timeout(Duration::from_secs(30), rollback)
            .await
            .expect("the rollback must finish")
            .unwrap();
        write_revocation_boundary!(row_after_write);
        assert!(
            !row_has_record(&manager, &a_account.id, &a_room, None, &nonce).await,
            "the release authorized the write"
        );
        assert!(
            live.drafts
                .record_status(&a_account.id, &a_room, None, &nonce)
                .is_none()
        );
        write_revocation_boundary!(switch_back);
        manager
            .switch_to(&a_account.id)
            .await
            .expect("Cannot switch back");
        let renewed = manager.account_selection(&a_account.id).unwrap();
        write_revocation_boundary!(recovery_discovery);
        live.adopt_recovered_sends(renewed, Weak::default()).await;
        write_revocation_boundary!(flush);
        live.flush_drafts().await.expect("the flush succeeds");
        assert!(live.drafts.dirty_keys().is_empty());
        write_revocation_boundary!(row_after_flush);
        assert!(!row_has_record(&manager, &a_account.id, &a_room, None, &nonce).await);

        write_revocation_boundary!(logout_first);
        manager
            .logout_account(&a_account.id)
            .await
            .expect("Cannot remove first account");
        write_revocation_boundary!(logout_second);
        manager
            .logout_account(&b_account.id)
            .await
            .expect("Cannot remove second account");
        write_revocation_boundary!(manager_stop);
        manager.stop().await;
        drop(manager);
        write_revocation_boundary!(profile_close);
        close_profile(directory, live).await;
        write_revocation_boundary!(complete);
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

#[cfg(test)]
mod join_tests {
    use super::*;

    /// A confirmed join changes nothing local and navigates nowhere until
    /// sync shows the room joined; then a room opens as a room and a Space
    /// as a Space, from what sync says, not from a directory hint.
    #[test]
    fn a_join_opens_nothing_until_sync_shows_the_room_joined() {
        assert_eq!(joined_navigation(None), None);
        assert_eq!(joined_navigation(Some(false)), Some(JoinedNavigation::Room));
        assert_eq!(joined_navigation(Some(true)), Some(JoinedNavigation::Space));
    }

    /// The generations are the click's: a security step that began after
    /// it refuses the hand-off even once it has ended, and one running at
    /// the click refuses it too.
    #[test]
    fn a_join_is_admitted_only_by_the_security_gate_as_it_was_at_the_click() {
        let gate = Arc::new(crate::security::SecurityGate::default());
        let asked = JoinAsked::at(&gate, 7);
        assert_eq!(asked.navigation, 7);
        assert!(gate.ringing_allowed(asked.security));
        // A step begins and ends while the join waits: the gate is idle now,
        // and the join is still refused.
        drop(gate.try_account_access().expect("the gate was idle"));
        assert!(!gate.blocked());
        assert!(!gate.ringing_allowed(asked.security));
        // Asked while a step runs: refused, also after it ends.
        let running = gate.try_account_access().expect("the gate was idle");
        let during = JoinAsked::at(&gate, 7);
        drop(running);
        assert!(!gate.ringing_allowed(during.security));
        // Asked now, with nothing since: admitted.
        assert!(gate.ringing_allowed(JoinAsked::at(&gate, 8).security));
    }

    /// A withdrawal before the hand-off sends nothing and revokes the open;
    /// a caller's own bound then reports nothing sent.
    #[test]
    fn a_withdrawn_join_opens_nothing_and_its_bound_reports_what_was_sent() {
        let gate = crate::security::SecurityGate::default();
        let asked = JoinAsked::at(&gate, 0);
        assert_eq!(asked.expired(), JoinError::Unavailable);
        assert!(asked.withdraw(), "nothing had been handed off");
        assert!(asked.authority.withdrawn(), "opening the room is withdrawn");
        assert!(asked.cancellation.is_cancelled() && !asked.cancellation.sent());
        assert_eq!(asked.expired(), JoinError::Unavailable);
        assert!(!JoinError::Unavailable.may_have_joined());
    }

    /// Unknown is not consumed by returning an error: Close/reopen or a
    /// discarded error callback is not an informed choice to send again.
    #[test]
    fn one_join_per_room_and_an_unknown_one_is_never_resent_unseen() {
        let outcomes = JoinOutcomes::default();
        let key = ("a".to_owned(), "!room:example.org".to_owned());
        let other_account = ("b".to_owned(), "!room:example.org".to_owned());
        let first = outcomes.begin(&key, || true).unwrap();
        assert_eq!(outcomes.begin(&key, || true), Err(JoinError::Pending));
        outcomes
            .begin(&other_account, || true)
            .expect("accounts are apart");
        // A definite answer forgets it.
        outcomes.settle(&key, first, false);
        assert_eq!(outcomes.get(&key), None);
        // An unknown one is kept whatever the UI shows or hides since.
        let uncertain = outcomes.begin(&key, || true).unwrap();
        outcomes.settle(&key, uncertain, true);
        assert_eq!(outcomes.get(&key), Some(JoinOutcome::Unknown));
        for _ in 0..3 {
            let earlier = outcomes.begin(&key, || true).unwrap_err();
            assert_eq!(earlier, JoinError::EarlierUnknown);
            assert!(earlier.to_string().contains("Nothing was sent"));
            assert!(earlier.to_string().contains("paused"));
            assert_eq!(outcomes.get(&key), Some(JoinOutcome::Unknown));
        }
        // A record dropped before its hand-off (a caller's bound) leaves
        // nothing behind: nothing was sent.
        let unsent_key = ("a".to_owned(), "!unsent:example.org".to_owned());
        let receipt = outcomes.begin(&unsent_key, || true).unwrap();
        let unsent = JoinCancellation::new();
        drop(JoinRecord::new(
            &outcomes,
            unsent_key.clone(),
            receipt,
            unsent,
        ));
        assert_eq!(outcomes.get(&unsent_key), None);
        // Bounded without evicting either uncertainty or in-flight work.
        for n in 0..MAX_JOIN_OUTCOMES - 2 {
            let another = ("c".to_owned(), format!("!r{n}:example.org"));
            let receipt = outcomes.begin(&another, || true).unwrap();
            outcomes.settle(&another, receipt, true);
        }
        assert_eq!(outcomes.outcomes().entries.len(), MAX_JOIN_OUTCOMES);
        assert_eq!(
            outcomes.begin(&unsent_key, || true),
            Err(JoinError::UnresolvedLimit)
        );
        assert_eq!(
            outcomes.begin(&key, || true),
            Err(JoinError::EarlierUnknown)
        );
        assert_eq!(outcomes.get(&key), Some(JoinOutcome::Unknown));
        assert_eq!(outcomes.get(&other_account), Some(JoinOutcome::InFlight));
    }

    #[test]
    fn account_removal_forgets_only_its_receipts_and_old_completions_cannot_reinsert() {
        let outcomes = JoinOutcomes::default();
        let a = ("a".to_owned(), "!room:example.org".to_owned());
        let b = ("b".to_owned(), "!room:example.org".to_owned());
        let old = outcomes.begin(&a, || true).unwrap();
        let other = outcomes.begin(&b, || true).unwrap();
        outcomes.settle(&b, other, true);
        outcomes.forget_account("a");
        outcomes.settle(&a, old, true);
        assert_eq!(
            outcomes.get(&a),
            None,
            "a late result cannot recreate removed data"
        );
        assert_eq!(outcomes.begin(&a, || false), Err(JoinError::AccountChanged));
        assert_eq!(
            outcomes.get(&a),
            None,
            "a revoked queued caller cannot insert after removal"
        );
        let renewed = outcomes.begin(&a, || true).unwrap();
        assert_ne!(old, renewed);
        outcomes.settle(&a, old, true);
        assert_eq!(outcomes.get(&a), Some(JoinOutcome::InFlight));
        drop(JoinRecord::new(
            &outcomes,
            a.clone(),
            old,
            JoinCancellation::new(),
        ));
        assert_eq!(outcomes.get(&a), Some(JoinOutcome::InFlight));
        outcomes.settle(&a, renewed, true);
        assert_eq!(outcomes.get(&a), Some(JoinOutcome::Unknown));
        assert_eq!(
            outcomes.get(&b),
            Some(JoinOutcome::Unknown),
            "other accounts are untouched"
        );
    }

    #[test]
    fn dropping_the_navigation_wait_withdraws_the_still_owned_open() {
        let asked = JoinAsked::at(&crate::security::SecurityGate::default(), 3);
        let waiting = JoinNavigationWait(asked.clone());
        assert!(!asked.authority.withdrawn());
        drop(waiting);
        assert!(asked.authority.withdrawn());
        assert!(asked.cancellation.is_cancelled());
    }
}
