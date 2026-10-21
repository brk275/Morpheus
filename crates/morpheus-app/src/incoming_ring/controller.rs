//! Bounded offer ownership and single-use Answer and Decline authorization.
//!
//! No discovery task, UI callback, slot write or media operation lives here.
//! The bounded worker supplies retained sync event IDs and cached change hints;
//! policy publication synchronously invalidates pending authority. SDK change
//! streams invalidate when observed, and Answer/postjoin use full revalidation.
//! Account switches retain this controller/ledger. Private integration is source
//! reviewed; real controller, watcher and media acceptance is still pending.
//! A decline from another trusted device of this user retires only its exact
//! offer (match_declines, then finish_decline after an owned trust check); it
//! never answers, joins or sends anything.

use super::{
    AdmissionFailure, DeclineOutcome, IncomingOffer,
    ledger::{OfferKey, OfferLedger},
    policy,
};
use crate::voice_spaces::VoiceSpaces;
use futures_util::FutureExt;
use matrix_sdk::{
    deserialized_responses::{AlgorithmInfo, EncryptionInfo, VerificationState},
    ruma::{EventId, RoomId},
};
use morpheus_matrix::{AccountManager, AccountSelection};
use morpheus_rtc::{
    CallInfo, CallOptions, InteropMode, ParticipantInfo, RtcService, ringing::MAX_RING_LIFETIME_MS,
};
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::Notify, time};

const MAX_OFFERS: usize = 4;
/// Own-device declines retained while their offer may still be admitted.
pub(crate) const MAX_DECLINE_RECEIPTS: usize = 16;
/// A retained decline never outlives the longest possible ring.
const DECLINE_RECEIPT_LIFETIME: Duration = Duration::from_millis(MAX_RING_LIFETIME_MS);
const MAX_RECEIPT_ID_BYTES: usize = 255;

/// The exact identity of one retained decline: this account, the room, the
/// declined notification and the declining device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReceiptKey {
    account: String,
    room: String,
    notification: String,
    device: String,
}

/// What ReceiptBuffer needs from a retained receipt.
pub(crate) trait BufferedReceipt {
    fn key(&self) -> &ReceiptKey;
    fn expires(&self) -> Instant;
    /// Still owned by the selection it was received under.
    fn current(&self) -> bool;
}

/// Bounded, short-lived, in-memory receipts: no store, channel or task.
/// Expired or no longer current receipts are dropped first. A duplicate of a
/// retained key is ignored (the first stays). When full, the newest receipt is
/// refused, never a retained one displaced: an offer then keeps ringing rather
/// than being settled on evidence that was not kept. Only receipts that passed
/// the receipt-time checks (decline_receipt_qualifies) ever reach it.
pub(crate) struct ReceiptBuffer<T> {
    entries: VecDeque<T>,
}
impl<T> Default for ReceiptBuffer<T> {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }
}
impl<T: BufferedReceipt> ReceiptBuffer<T> {
    fn prune(&mut self, now: Instant) {
        self.entries
            .retain(|entry| now < entry.expires() && entry.current());
    }
    pub(crate) fn insert(&mut self, receipt: T, now: Instant) -> bool {
        self.prune(now);
        if !(now < receipt.expires() && receipt.current())
            || self
                .entries
                .iter()
                .any(|entry| entry.key() == receipt.key())
            || self.entries.len() >= MAX_DECLINE_RECEIPTS
        {
            return false;
        }
        self.entries.push_back(receipt);
        true
    }
    /// Every live receipt, oldest first, leaving the buffer empty.
    pub(crate) fn take(&mut self, now: Instant) -> Vec<T> {
        self.prune(now);
        self.entries.drain(..).collect()
    }
    /// Put back receipts taken earlier that matched no offer yet. They are
    /// older than anything received meanwhile, so they come first; the bound
    /// then drops the newest.
    pub(crate) fn restore(&mut self, earlier: Vec<T>, now: Instant) {
        let newer = std::mem::take(&mut self.entries);
        for receipt in earlier.into_iter().chain(newer) {
            self.insert(receipt, now);
        }
    }
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The scope that may hold retained receipts: the selected account.
pub(crate) trait ReceiptScope {
    fn scope_account(&self) -> &str;
    fn scope_current(&self) -> bool;
}
impl ReceiptScope for AccountSelection {
    fn scope_account(&self) -> &str {
        self.account_id()
    }
    fn scope_current(&self) -> bool {
        self.is_current()
    }
}

/// Retained receipts and the scope allowed to hold them, under ONE lock (the
/// owner's Mutex), so admission, insertion, restoration and clearing are
/// serialized. A receipt enters (from a sync callback, or back after a
/// settlement pass) only while not stopped and only if its own captured
/// selection belongs to the current scope: the same account, both still
/// current (a current selection is the account's only current one). reset
/// and stop clear in the same critical section that changes the scope, so a
/// callback dispatched before them cannot refill the buffer afterwards. No
/// fresh authority is ever captured for an old callback.
pub(crate) struct ScopedReceipts<T, S> {
    stopped: bool,
    scope: Option<S>,
    buffer: ReceiptBuffer<T>,
}
impl<T, S> Default for ScopedReceipts<T, S> {
    fn default() -> Self {
        Self {
            stopped: false,
            scope: None,
            buffer: ReceiptBuffer::default(),
        }
    }
}
impl<T: BufferedReceipt, S: ReceiptScope> ScopedReceipts<T, S> {
    fn admits(&self, receipt: &T) -> bool {
        !self.stopped
            && receipt.current()
            && self.scope.as_ref().is_some_and(|scope| {
                scope.scope_current() && scope.scope_account() == receipt.key().account
            })
    }
    /// A new scope (or none): everything retained is dropped, unless the new
    /// scope is the same, still current selection as the old one (the scope
    /// set before its handlers, then confirmed by selection).
    pub(crate) fn reset(&mut self, scope: Option<S>) {
        let same = match (&self.scope, &scope) {
            (Some(old), Some(new)) => {
                old.scope_current()
                    && new.scope_current()
                    && old.scope_account() == new.scope_account()
            }
            _ => false,
        };
        if !same || self.stopped {
            self.buffer.clear();
        }
        self.scope = if self.stopped { None } else { scope };
    }
    /// For good: nothing is retained or admitted afterwards.
    pub(crate) fn stop(&mut self) {
        self.stopped = true;
        self.scope = None;
        self.buffer.clear();
    }
    pub(crate) fn insert(&mut self, receipt: T, now: Instant) -> bool {
        self.admits(&receipt) && self.buffer.insert(receipt, now)
    }
    /// Every live receipt still admitted by the current scope.
    pub(crate) fn take(&mut self, now: Instant) -> Vec<T> {
        let taken = self.buffer.take(now);
        taken
            .into_iter()
            .filter(|receipt| self.admits(receipt))
            .collect()
    }
    /// Receipts taken earlier that matched no offer: back only if the same
    /// scope still admits them.
    pub(crate) fn restore(&mut self, earlier: Vec<T>, now: Instant) {
        let earlier = earlier
            .into_iter()
            .filter(|receipt| self.admits(receipt))
            .collect();
        self.buffer.restore(earlier, now);
    }
    #[cfg(test)]
    fn len(&self) -> usize {
        self.buffer.len()
    }
}

/// The identity of one offer that a decline must match exactly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OfferFacts<'a> {
    account: &'a str,
    room: &'a str,
    notification: &'a str,
    own_user: &'a str,
    own_device: &'a str,
}
impl<'a> OfferFacts<'a> {
    fn of(offer: &'a IncomingOffer) -> Self {
        Self {
            account: offer.selection.account_id(),
            room: offer.room.room_id().as_str(),
            notification: offer.ring.event_id(),
            own_user: &offer.own_user,
            own_device: &offer.own_device,
        }
    }
    fn owned(&self) -> OwnedOfferFacts {
        OwnedOfferFacts {
            account: self.account.to_owned(),
            room: self.room.to_owned(),
            notification: self.notification.to_owned(),
            own_user: self.own_user.to_owned(),
            own_device: self.own_device.to_owned(),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct OwnedOfferFacts {
    account: String,
    room: String,
    notification: String,
    own_user: String,
    own_device: String,
}
impl OwnedOfferFacts {
    fn borrowed(&self) -> OfferFacts<'_> {
        OfferFacts {
            account: &self.account,
            room: &self.room,
            notification: &self.notification,
            own_user: &self.own_user,
            own_device: &self.own_device,
        }
    }
}

/// A decline (its key and decrypted sender) matches exactly this offer: the
/// same account, room and retained notification, sent by the offer's own
/// user from a device other than the offer's own.
fn decline_key_matches(key: &ReceiptKey, sender: &str, offer: &OfferFacts<'_>) -> bool {
    key.account == offer.account
        && key.room == offer.room
        && key.notification == offer.notification
        && sender == offer.own_user
        && key.device != offer.own_device
}
fn decline_matches(receipt: &DeclineReceipt, offer: &OfferFacts<'_>) -> bool {
    decline_key_matches(&receipt.key, receipt.encryption.sender.as_str(), offer)
}

/// After a trust check: whether its decline may still retire the offer. Every
/// original condition is required again at this moment, including the
/// receipt's own original deadline (independent of the offer's).
pub(super) fn decline_still_settles(
    trusted: bool,
    receipt_current: bool,
    receipt_expires: Instant,
    now: Instant,
    offer_pending: bool,
    facts_match: bool,
) -> bool {
    trusted && receipt_current && now < receipt_expires && offer_pending && facts_match
}

/// One decline's current-trust check, owning everything it needs (the
/// original token, the offer's identity facts and client, and the receipt),
/// so it borrows no controller state while it waits.
pub(crate) struct DeclineCheck {
    token: ActionToken,
    facts: OwnedOfferFacts,
    receipt: DeclineReceipt,
    client: matrix_sdk::Client,
}
impl fmt::Debug for DeclineCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeclineCheck { .. }")
    }
}
impl DeclineCheck {
    /// The device's current trust with the keys the decline was decrypted
    /// with (require_current_sender, bounded by its own timeout).
    pub(crate) fn trust(self) -> impl Future<Output = (Self, bool)> + Send + 'static {
        async move {
            let trusted = policy::require_current_sender(&self.client, &self.receipt.encryption)
                .await
                .is_ok();
            (self, trusted)
        }
    }
}

/// Receipt-time checks for a decline the active account's sync delivered: the
/// event's own decryption metadata says it was Megolm-encrypted with a known
/// session, verified at decryption, not decrypted with forwarded (shared on
/// invite) keys, and sent by this same user from another device. This is
/// evidence only, never trust to act: finish_decline retires nothing until a
/// DeclineCheck has confirmed the device's current trust.
pub(crate) fn decline_receipt_qualifies(
    own_user: &str,
    own_device: &str,
    sender: &str,
    encryption: Option<&EncryptionInfo>,
) -> bool {
    let Some(encryption) = encryption else {
        return false;
    };
    let session = matches!(
        &encryption.algorithm_info,
        AlgorithmInfo::MegolmV1AesSha2 { session_id: Some(session), .. } if !session.is_empty()
    );
    session
        && encryption.verification_state == VerificationState::Verified
        && encryption.forwarder.is_none()
        && !own_user.is_empty()
        && !own_device.is_empty()
        && sender == own_user
        && encryption.sender.as_str() == own_user
        && encryption
            .sender_device
            .as_deref()
            .is_some_and(|device| device.as_str() != own_device)
}

/// A decline of one notification by another device of this account's own
/// user, as the account's sync delivered it, with its real decryption
/// metadata. It passed the receipt-time checks only; it is not trusted to act
/// until a DeclineCheck has checked the device's current trust.
#[derive(Clone)]
pub(crate) struct DeclineReceipt {
    selection: AccountSelection,
    key: ReceiptKey,
    encryption: EncryptionInfo,
    expires: Instant,
}
impl fmt::Debug for DeclineReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeclineReceipt { .. }")
    }
}
impl BufferedReceipt for DeclineReceipt {
    fn key(&self) -> &ReceiptKey {
        &self.key
    }
    fn expires(&self) -> Instant {
        self.expires
    }
    fn current(&self) -> bool {
        self.selection.is_current()
    }
}
impl DeclineReceipt {
    /// None unless the receipt-time checks pass, the selection it arrived
    /// under is still current and every identifier is bounded.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_sync(
        selection: &AccountSelection,
        own_user: &str,
        own_device: &str,
        room_id: &str,
        sender: &str,
        notification_id: &str,
        encryption: Option<&EncryptionInfo>,
        now: Instant,
    ) -> Option<Self> {
        let bounded = |id: &str| !id.is_empty() && id.len() <= MAX_RECEIPT_ID_BYTES;
        if !selection.is_current()
            || !bounded(room_id)
            || !bounded(notification_id)
            || !decline_receipt_qualifies(own_user, own_device, sender, encryption)
        {
            return None;
        }
        let encryption = encryption?.clone();
        let device = encryption.sender_device.as_deref()?.to_string();
        Some(Self {
            selection: selection.clone(),
            key: ReceiptKey {
                account: selection.account_id().to_owned(),
                room: room_id.to_owned(),
                notification: notification_id.to_owned(),
                device,
            },
            encryption,
            expires: now.checked_add(DECLINE_RECEIPT_LIFETIME)?,
        })
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct ActionToken(u64);
impl ActionToken {
    /// Display-only key. UI keeps the corresponding opaque token; text cannot
    /// construct authorization or select a different room.
    pub(crate) fn ui_key(self) -> String {
        format!("incoming-{}", self.0)
    }
}
impl fmt::Debug for ActionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ActionToken { .. }")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControllerFailure {
    Unavailable,
    Capacity,
    Stale,
    Expired,
    Busy,
    Revoked,
    Admission(AdmissionFailure),
    WrongCall,
}

/// Owned presentation data. This never supplies the join target back to Answer:
/// Answer takes only the opaque token and consumes the stored admitted offer.
#[derive(Clone)]
pub(crate) struct OfferPresentation {
    pub token: ActionToken,
    pub account_id: String,
    pub room_id: String,
    pub caller_user_id: String,
    pub mode: InteropMode,
    pub audible: AudibleOffer,
}
impl fmt::Debug for OfferPresentation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OfferPresentation { .. }")
    }
}

/// Non-forgeable, single-use audible authority derived only from an admitted
/// stored offer. Cloned presentations share the same claim/stop state. Callback
/// checks use only immutable data and atomics; no room IDs or UI keys are used.
#[derive(Clone)]
pub(crate) struct AudibleOffer {
    selection: AccountSelection,
    sound: Arc<AudibleState>,
    signal: Arc<Signal>,
    epoch: u64,
}
struct AudibleState {
    alive: AtomicBool,
    claimed: AtomicBool,
    deadline: Instant,
}
impl AudibleState {
    fn new(deadline: Instant) -> Self {
        Self {
            alive: AtomicBool::new(true),
            claimed: AtomicBool::new(false),
            deadline,
        }
    }
    fn claim(&self) -> bool {
        !self.claimed.swap(true, Ordering::AcqRel)
    }
    fn stop(&self) {
        self.alive.store(false, Ordering::Release);
    }
    fn current(&self, epoch_current: bool, selection_current: bool, now: Instant) -> bool {
        self.alive.load(Ordering::Acquire)
            && epoch_current
            && selection_current
            && now < self.deadline
    }
}
impl AudibleOffer {
    pub(crate) fn claim(&self) -> bool {
        self.sound.claim()
    }
    pub(crate) fn stop(&self) {
        self.sound.stop();
    }
    pub(crate) fn deadline(&self) -> Instant {
        self.sound.deadline
    }
    pub(crate) fn allowed(&self) -> bool {
        self.sound.current(
            self.signal.audio_epoch.load(Ordering::Acquire) == self.epoch,
            self.selection.is_current(),
            Instant::now(),
        )
    }
}

struct SignalState {
    epoch: u64,
    stopped: bool,
}
struct Signal {
    audio_epoch: AtomicU64,
    state: Mutex<SignalState>,
    changed: Notify,
}
impl Default for Signal {
    fn default() -> Self {
        Self {
            audio_epoch: AtomicU64::new(1),
            state: Mutex::new(SignalState {
                epoch: 1,
                stopped: false,
            }),
            changed: Notify::new(),
        }
    }
}

/// Cheap synchronous withdrawal, including while the actor is awaiting SDK I/O
/// or Answer is waiting on the account-transition lock. Any missed/ambiguous
/// change invalidates all pending offers; it cannot extend their deadlines.
#[derive(Clone)]
pub(crate) struct InvalidationHandle(Arc<Signal>);
impl InvalidationHandle {
    pub(crate) fn invalidate(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(|error| {
            let mut state = error.into_inner();
            state.stopped = true;
            state
        });
        if let Some(next) = state.epoch.checked_add(1) {
            state.epoch = next;
        } else {
            state.stopped = true;
        }
        self.0.audio_epoch.store(
            if state.stopped { 0 } else { state.epoch },
            Ordering::Release,
        );
        drop(state);
        self.0.changed.notify_waiters();
    }
    pub(crate) fn shutdown(&self) {
        self.0.audio_epoch.store(0, Ordering::Release);
        self.0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stopped = true;
        self.0.changed.notify_waiters();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Offered,
    Reserved,
    Ready,
    Joining,
    Established,
    /// One explicit Decline owns the offer: no Answer, no ringing, and it
    /// stays pending only until its scope or deadline ends.
    Declining,
    Revoked,
}
struct LeaseState {
    phase: Phase,
    deadline: Instant,
}
pub(super) struct Lease {
    epoch: u64,
    audible: Arc<AudibleState>,
    state: Mutex<LeaseState>,
    changed: Notify,
}
impl Lease {
    pub(super) fn new(epoch: u64, deadline: Instant) -> Self {
        Self {
            epoch,
            audible: Arc::new(AudibleState::new(deadline)),
            state: Mutex::new(LeaseState {
                phase: Phase::Offered,
                deadline,
            }),
            changed: Notify::new(),
        }
    }
    fn revoke(&self) {
        self.audible.stop();
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .phase = Phase::Revoked;
        self.changed.notify_waiters();
    }
    fn deadline(&self) -> Option<Instant> {
        self.state.lock().ok().map(|state| state.deadline)
    }
    fn pending(&self, signal: &Signal, now: Instant, selection_current: bool) -> bool {
        let Ok(global) = signal.state.lock() else {
            return false;
        };
        let Ok(state) = self.state.lock() else {
            return false;
        };
        selection_current
            && !global.stopped
            && self.epoch == global.epoch
            && now < state.deadline
            && matches!(
                state.phase,
                Phase::Offered | Phase::Reserved | Phase::Ready | Phase::Joining | Phase::Declining
            )
    }
    fn advance(
        &self,
        signal: &Signal,
        now: Instant,
        selection_current: bool,
        from: Phase,
        to: Phase,
        deadline: Instant,
    ) -> Result<(), ControllerFailure> {
        self.audible.stop();
        // Serialize commitment with invalidation. A signal observed before this
        // transition cannot be discarded as "the call is already established".
        let global = signal
            .state
            .lock()
            .map_err(|_| ControllerFailure::Revoked)?;
        let mut state = self.state.lock().map_err(|_| ControllerFailure::Revoked)?;
        state.deadline = state.deadline.min(deadline);
        if global.stopped || global.epoch != self.epoch || !selection_current {
            state.phase = Phase::Revoked;
            return Err(ControllerFailure::Revoked);
        }
        if now >= state.deadline {
            state.phase = Phase::Revoked;
            return Err(ControllerFailure::Expired);
        }
        if state.phase != from {
            return Err(ControllerFailure::Stale);
        }
        state.phase = to;
        drop(state);
        drop(global);
        self.changed.notify_waiters();
        Ok(())
    }
    fn established(&self, signal: &Signal, selection_current: bool) -> bool {
        let Ok(global) = signal.state.lock() else {
            return false;
        };
        let Ok(state) = self.state.lock() else {
            return false;
        };
        // Ordinary ring expiry, withdrawal and policy changes after commitment
        // do not hang up an established call. Account/shutdown/owner Drop do.
        selection_current && !global.stopped && state.phase == Phase::Established
    }
}

pub(super) struct Pending<T> {
    payload: T,
    lease: Arc<Lease>,
    /// Published at least once (shown, and possibly rung).
    presented: bool,
    /// Devices whose decline of this offer already began a trust check,
    /// whatever its outcome. A replay of the same decline (the SDK does not
    /// deduplicate re-delivered events across sync responses) never starts
    /// another check or renews withholding. Bounded, and gone with the offer.
    checked: Vec<String>,
}
/// Distinct declining devices checked per offer; others are ignored.
const MAX_CHECKED_DEVICES: usize = 8;
pub(super) struct OfferBook<T> {
    entries: BTreeMap<ActionToken, Pending<T>>,
    next: u64,
    /// Decline trust checks in flight per offer (bounded by the receipts).
    checking: BTreeMap<ActionToken, usize>,
}
impl<T> Default for OfferBook<T> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            next: 1,
            checking: BTreeMap::new(),
        }
    }
}
impl<T> OfferBook<T> {
    pub(super) fn insert(
        &mut self,
        payload: T,
        lease: Arc<Lease>,
    ) -> Result<ActionToken, ControllerFailure> {
        if self.entries.len() >= MAX_OFFERS {
            return Err(ControllerFailure::Capacity);
        }
        let token = ActionToken(self.next);
        self.next = self
            .next
            .checked_add(1)
            .ok_or(ControllerFailure::Capacity)?;
        self.entries.insert(
            token,
            Pending {
                payload,
                lease,
                presented: false,
                checked: Vec::new(),
            },
        );
        Ok(token)
    }
    pub(super) fn take(&mut self, token: ActionToken) -> Result<Pending<T>, ControllerFailure> {
        self.checking.remove(&token);
        self.entries.remove(&token).ok_or(ControllerFailure::Stale)
    }
    /// Every entry, with no check left behind (scope change, stop).
    fn drain(&mut self) -> BTreeMap<ActionToken, Pending<T>> {
        self.checking.clear();
        std::mem::take(&mut self.entries)
    }
    /// Begin a decline trust check for `token` from `device`. False (and
    /// nothing changes) if the offer is gone, that device's decline of it was
    /// already checked or is being checked, or it already had its bounded
    /// number of distinct devices: a replayed or excess decline is consumed
    /// without a check and cannot extend withholding.
    pub(super) fn begin_check(&mut self, token: ActionToken, device: &str) -> bool {
        let Some(pending) = self.entries.get_mut(&token) else {
            return false;
        };
        if pending.checked.iter().any(|checked| checked == device)
            || pending.checked.len() >= MAX_CHECKED_DEVICES
        {
            return false;
        }
        pending.checked.push(device.to_owned());
        *self.checking.entry(token).or_default() += 1;
        true
    }
    pub(super) fn end_check(&mut self, token: ActionToken) {
        if let Some(count) = self.checking.get_mut(&token) {
            *count -= 1;
            if *count == 0 {
                self.checking.remove(&token);
            }
        }
    }
    /// Not yet shown and awaiting a decline's trust check: not published
    /// until the check ends, so an authenticated decline that arrived first
    /// retires it before it is ever shown or rung. An offer already shown
    /// keeps ringing until a decline is authenticated.
    pub(super) fn withheld(&self, token: ActionToken) -> bool {
        self.checking.contains_key(&token)
            && self
                .entries
                .get(&token)
                .is_some_and(|pending| !pending.presented)
    }
    /// Published (shown, and possibly rung) from now on.
    pub(super) fn mark_presented(&mut self, token: ActionToken) {
        if let Some(pending) = self.entries.get_mut(&token) {
            pending.presented = true;
        }
    }
}

pub(crate) struct IncomingController {
    /// The one replay ledger. Shared only with the admission in progress
    /// (begin_receive / finish_receive), which locks it per call and never
    /// across an await, so settlement can run while that admission waits.
    ledger: Arc<Mutex<OfferLedger>>,
    book: OfferBook<IncomingOffer>,
    signal: Arc<Signal>,
    selected: Option<AccountSelection>,
    accepting: Arc<AtomicBool>,
}

/// What an admission captured at its start: the selection and the
/// controller epoch it began under. finish_receive uses these, never newer
/// ones.
pub(crate) struct ReceiveTicket {
    selection: AccountSelection,
    epoch: u64,
}
impl fmt::Debug for ReceiveTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReceiveTicket { .. }")
    }
}
impl Default for IncomingController {
    fn default() -> Self {
        Self {
            ledger: Arc::new(Mutex::new(OfferLedger::default())),
            book: OfferBook::default(),
            signal: Arc::new(Signal::default()),
            selected: None,
            accepting: Arc::new(AtomicBool::new(false)),
        }
    }
}
impl Drop for IncomingController {
    fn drop(&mut self) {
        self.invalidation_handle().shutdown();
    }
}
impl IncomingController {
    pub(crate) fn invalidation_handle(&self) -> InvalidationHandle {
        InvalidationHandle(Arc::clone(&self.signal))
    }
    pub(crate) fn select(&mut self, selection: Option<AccountSelection>) {
        self.invalidation_handle().invalidate();
        self.clear_offers();
        self.selected = selection;
    }
    fn retire(&mut self, offer: &IncomingOffer) {
        // Admission already reserved this key, so retiring cannot need capacity.
        let _ = self
            .ledger
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retire(OfferKey::new(
                offer.selection.account_id(),
                offer.room.room_id().as_str(),
                offer.ring.event_id(),
            ));
    }
    /// The shared replay ledger, for the admission begun by begin_receive.
    pub(crate) fn ledger(&self) -> Arc<Mutex<OfferLedger>> {
        Arc::clone(&self.ledger)
    }
    fn clear_offers(&mut self) {
        let entries = self.book.drain();
        for (_, pending) in entries {
            pending.lease.revoke();
            self.retire(&pending.payload);
        }
    }
    pub(crate) fn reap(&mut self) {
        let now = Instant::now();
        let stale: Vec<_> = self
            .book
            .entries
            .iter()
            .filter(|(_, pending)| {
                !pending
                    .lease
                    .pending(&self.signal, now, pending.payload.selection.is_current())
            })
            .map(|(token, _)| *token)
            .collect();
        for token in stale {
            if let Ok(pending) = self.book.take(token) {
                pending.lease.revoke();
                self.retire(&pending.payload);
            }
        }
    }
    /// Serialized admission, at most four live observers. The SDK must retain
    /// this event as an unsolicited sync receipt; arbitrary event fetches cannot
    /// create an offer. Intake queues/room discovery belong to the future worker.
    ///
    /// It runs in three steps so no controller borrow spans its awaits:
    /// begin_receive (synchronous: selection, epoch, capacity), then the
    /// caller's one IncomingOffer::admit future with this controller's shared
    /// ledger (its one-time observation), then finish_receive (synchronous:
    /// the original selection and epoch, then the book). Only the incoming
    /// actor admits, one admission at a time, and only removals (settlement,
    /// expiry) can happen to the book in between.
    pub(crate) fn begin_receive(
        &mut self,
        room: &RoomId,
        event: &EventId,
    ) -> Result<ReceiveTicket, ControllerFailure> {
        self.reap();
        let selection = self
            .selected
            .as_ref()
            .filter(|s| s.is_current())
            .ok_or(ControllerFailure::Unavailable)?
            .clone();
        let epoch = {
            let state = self
                .signal
                .state
                .lock()
                .map_err(|_| ControllerFailure::Revoked)?;
            if state.stopped {
                return Err(ControllerFailure::Revoked);
            }
            state.epoch
        };
        if self.accepting.load(Ordering::Acquire) || self.book.entries.len() >= MAX_OFFERS {
            self.ledger
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .retire(OfferKey::new(
                    selection.account_id(),
                    room.as_str(),
                    event.as_str(),
                ))
                .map_err(|_| ControllerFailure::Capacity)?;
            return Err(ControllerFailure::Capacity);
        }
        Ok(ReceiveTicket { selection, epoch })
    }
    /// The end of the admission begun by begin_receive, with its result.
    pub(crate) fn finish_receive(
        &mut self,
        ticket: ReceiveTicket,
        admitted: Result<IncomingOffer, AdmissionFailure>,
    ) -> Result<ActionToken, ControllerFailure> {
        let ReceiveTicket { selection, epoch } = ticket;
        let offer = admitted.map_err(ControllerFailure::Admission)?;
        let lease = Arc::new(Lease::new(epoch, offer.deadline()));
        if offer.selection.account_id() != selection.account_id()
            || !selection.is_current()
            || !lease.pending(&self.signal, Instant::now(), offer.selection.is_current())
        {
            self.retire(&offer);
            return Err(ControllerFailure::Revoked);
        }
        self.book.insert(offer, lease)
    }
    pub(crate) fn presentations(&mut self) -> Vec<OfferPresentation> {
        self.reap();
        let shown: Vec<_> = self
            .book
            .entries
            .keys()
            .copied()
            .filter(|token| !self.book.withheld(*token))
            .collect();
        for token in &shown {
            self.book.mark_presented(*token);
        }
        self.book
            .entries
            .iter()
            .filter(|(token, _)| shown.contains(token))
            .map(|(token, pending)| OfferPresentation {
                token: *token,
                account_id: pending.payload.selection.account_id().to_owned(),
                room_id: pending.payload.room.room_id().to_string(),
                caller_user_id: pending.payload.ring.sender().to_owned(),
                mode: pending.payload.mode(),
                audible: AudibleOffer {
                    selection: pending.payload.selection.clone(),
                    sound: pending.lease.audible.clone(),
                    signal: self.signal.clone(),
                    epoch: pending.lease.epoch,
                },
            })
            .collect()
    }
    /// Local dismissal only. No protocol Decline is sent or claimed.
    pub(crate) fn dismiss(&mut self, token: ActionToken) -> Result<(), ControllerFailure> {
        let pending = self.book.take(token)?;
        pending.lease.revoke();
        self.retire(&pending.payload);
        Ok(())
    }
    /// Explicit protocol Decline. Consumes the token synchronously, retires
    /// its ledger key (the offer is never shown or answered again) and stops
    /// its ringing, keeping the offer under its original lease, epoch and
    /// selection. Nothing is sent here: DeclineClaim::send makes the one
    /// bounded attempt, and dropping the claim revokes it.
    pub(crate) fn decline(
        &mut self,
        token: ActionToken,
    ) -> Result<DeclineClaim, ControllerFailure> {
        self.reap();
        let pending = self.book.take(token)?;
        self.retire(&pending.payload);
        if let Err(failure) = pending.lease.advance(
            &self.signal,
            Instant::now(),
            pending.payload.selection.is_current(),
            Phase::Offered,
            Phase::Declining,
            pending.payload.deadline(),
        ) {
            pending.lease.revoke();
            return Err(failure);
        }
        Ok(DeclineClaim {
            offer: pending.payload,
            lease: pending.lease,
            signal: Arc::clone(&self.signal),
        })
    }
    /// Retire each offer that another trusted device of the same user has
    /// declined, exactly as dismiss does (lease revoked, so its ringing, row
    /// and banner end; ledger key retired, so it is never offered again).
    /// Nothing is sent, joined, left or opened. A receipt settles an offer
    /// only when it is still current and unexpired and matches an offer in
    /// the book (so not one already being answered or declined here) with:
    /// - the same account, under a selection that is still current;
    /// - exactly the offer's room and retained notification;
    /// - the offer's own user as sender, from a device other than its own;
    /// - that device still current and cross-signing trusted now, with the
    ///   same keys the decline was decrypted with (require_current_sender).
    ///
    /// This runs in two synchronous steps around an owned trust check, so the
    /// actor keeps serving commands while a check waits (no controller borrow
    /// is held across it):
    /// - match_declines pairs each live receipt with its exact offer and
    ///   returns a DeclineCheck that captured the original token and facts;
    ///   the offer is withheld from presentation meanwhile if it was not yet
    ///   shown (OfferBook::withheld);
    /// - finish_decline, after the check, revalidates every original fact
    ///   (including the receipt's own deadline) and only then dismisses.
    ///
    /// A matched receipt is consumed whether or not it settled the offer; a
    /// failed trust check is never retried. At most `room` checks are begun;
    /// receipts that match no offer yet (its admission may still be running),
    /// or exceed `room`, are returned for retention.
    pub(crate) fn match_declines(
        &mut self,
        receipts: Vec<DeclineReceipt>,
        room: usize,
    ) -> (Vec<DeclineCheck>, Vec<DeclineReceipt>) {
        self.reap();
        let now = Instant::now();
        let mut checks = Vec::new();
        let mut unmatched = Vec::new();
        for receipt in receipts {
            if !receipt.current() || now >= receipt.expires() {
                continue;
            }
            let found = (checks.len() < room)
                .then(|| {
                    self.book.entries.iter().find_map(|(token, pending)| {
                        let offer = &pending.payload;
                        let facts = OfferFacts::of(offer);
                        (offer.selection.is_current() && decline_matches(&receipt, &facts))
                            .then(|| (*token, facts.owned(), offer.client.clone()))
                    })
                })
                .flatten();
            let Some((token, facts, client)) = found else {
                unmatched.push(receipt);
                continue;
            };
            // A replay of a decline already checked (or being checked) for
            // this offer is consumed here without a second check.
            if self.book.begin_check(token, &receipt.key.device) {
                checks.push(DeclineCheck {
                    token,
                    facts,
                    receipt,
                    client,
                });
            }
        }
        (checks, unmatched)
    }
    /// The end of one decline's trust check. The offer is dismissed only if
    /// the device was trusted, the receipt is still current and before its
    /// own original deadline, and the same token is still a pending book
    /// offer of a current selection with exactly the captured facts.
    pub(crate) fn finish_decline(&mut self, check: DeclineCheck, trusted: bool) -> bool {
        self.book.end_check(check.token);
        let now = Instant::now();
        let settles = self.book.entries.get(&check.token).is_some_and(|pending| {
            let offer = &pending.payload;
            decline_still_settles(
                trusted,
                check.receipt.current(),
                check.receipt.expires(),
                now,
                offer.selection.is_current()
                    && pending
                        .lease
                        .pending(&self.signal, now, offer.selection.is_current()),
                OfferFacts::of(offer) == check.facts.borrowed()
                    && decline_matches(&check.receipt, &check.facts.borrowed()),
            )
        });
        if settles {
            let _ = self.dismiss(check.token);
        }
        settles
    }
    /// Retained for explicit diagnostics only. The live worker uses cached
    /// hints: cancelling this network pass would revoke an offer being answered.
    #[allow(dead_code)]
    pub(crate) async fn reconcile(&mut self, manager: &AccountManager, preferences: &VoiceSpaces) {
        self.reap();
        let tokens: Vec<_> = self.book.entries.keys().copied().collect();
        for token in tokens {
            let valid = if let Some(pending) = self.book.entries.get_mut(&token) {
                let selection = pending.payload.selection.clone();
                let result = tokio::select! {
                    biased;
                    _ = wait_invalid(&pending.lease, &self.signal, &selection) => Err(AdmissionFailure::Revoked),
                    result = pending.payload.revalidate(manager, preferences) => result,
                };
                result.is_ok()
                    && pending
                        .lease
                        .advance(
                            &self.signal,
                            Instant::now(),
                            selection.is_current(),
                            Phase::Offered,
                            Phase::Offered,
                            pending.payload.deadline(),
                        )
                        .is_ok()
            } else {
                false
            };
            if !valid {
                let _ = self.dismiss(token);
            }
        }
    }
    /// Consumes a token synchronously before waiting on the account-transition
    /// lock or any network work. Never obtains the selected conversation/mode.
    pub(crate) fn accept(
        &mut self,
        token: ActionToken,
    ) -> Result<AcceptedOffer, ControllerFailure> {
        self.reap();
        if self
            .accepting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(ControllerFailure::Busy);
        }
        let permit = AcceptPermit(Arc::clone(&self.accepting));
        let pending = self.book.take(token)?;
        self.retire(&pending.payload);
        pending.lease.advance(
            &self.signal,
            Instant::now(),
            pending.payload.selection.is_current(),
            Phase::Offered,
            Phase::Reserved,
            pending.payload.deadline(),
        )?;
        Ok(AcceptedOffer {
            offer: pending.payload,
            lease: pending.lease,
            signal: Arc::clone(&self.signal),
            _permit: permit,
            handed_off: false,
        })
    }
    pub(crate) fn shutdown(&mut self) {
        self.invalidation_handle().shutdown();
        self.clear_offers();
        self.selected = None;
    }
}

struct AcceptPermit(Arc<AtomicBool>);
impl Drop for AcceptPermit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
struct WorkGuard {
    lease: Arc<Lease>,
    complete: bool,
}
impl Drop for WorkGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.lease.revoke();
        }
    }
}

/// One explicit Decline's single-use ownership of an offer, out of the book.
/// It keeps the click's original offer (account, room, notification, receipt
/// generation, selection) and lease. Dropping it, for example when its UI
/// task is aborted at shutdown, revokes the lease; nothing is queued or
/// retried afterwards.
pub(crate) struct DeclineClaim {
    offer: IncomingOffer,
    lease: Arc<Lease>,
    signal: Arc<Signal>,
}
impl fmt::Debug for DeclineClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeclineClaim { .. }")
    }
}
impl Drop for DeclineClaim {
    fn drop(&mut self) {
        self.lease.revoke();
    }
}
impl DeclineClaim {
    pub(crate) fn selection(&self) -> &AccountSelection {
        &self.offer.selection
    }
    /// The one bounded attempt. `allowed` is the click's own fence (its
    /// security generation); with the lease, epoch and original selection it
    /// is checked before the trust check, the fetch and the send. The scope
    /// ending (account switch, lock, removal, policy change, worker stop,
    /// expiry) cancels the attempt: before the send that is NotSent, during
    /// it Unknown. Never reacquires authority.
    pub(crate) async fn send(
        mut self,
        manager: &AccountManager,
        preferences: &VoiceSpaces,
        allowed: impl Fn() -> bool + Send + Sync,
    ) -> DeclineOutcome {
        let selection = self.offer.selection.clone();
        let lease = Arc::clone(&self.lease);
        let signal = Arc::clone(&self.signal);
        let sending = AtomicBool::new(false);
        let current =
            || allowed() && lease.pending(&signal, Instant::now(), selection.is_current());
        let outcome = tokio::select! {
            biased;
            _ = wait_invalid(&lease, &signal, &selection) => None,
            outcome = self.offer.decline(manager, preferences, current, &sending) => Some(outcome),
        };
        self.lease.revoke();
        outcome.unwrap_or(if sending.load(Ordering::Acquire) {
            DeclineOutcome::Unknown
        } else {
            DeclineOutcome::NotSent
        })
    }
}

/// Immutable target for the existing call worker's join. Camera stays off and
/// intent must be Quiet. The worker must use these options directly and must
/// not open a slot, read a preferred mode or silently fall back to another one.
pub(crate) struct JoinTarget {
    room_id: String,
    options: CallOptions,
    selection: AccountSelection,
}
impl JoinTarget {
    pub(crate) fn room_id(&self) -> &str {
        &self.room_id
    }
    pub(crate) fn options(&self) -> &CallOptions {
        &self.options
    }
    pub(crate) fn selection(&self) -> &AccountSelection {
        &self.selection
    }
}
impl fmt::Debug for JoinTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JoinTarget { .. }")
    }
}

/// Cloneable cancellation facts only. IncomingOffer owns a Send-only receipt
/// drain, so neither this guard nor its future borrows the full offer.
#[derive(Clone)]
pub(crate) struct PendingGuard {
    lease: Arc<Lease>,
    signal: Arc<Signal>,
    selection: AccountSelection,
}
impl PendingGuard {
    pub(crate) async fn cancelled(&self) {
        wait_invalid(&self.lease, &self.signal, &self.selection).await;
    }
}
impl fmt::Debug for PendingGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PendingGuard { .. }")
    }
}

/// Owned by the caller awaiting Answer startup. Dropping the UI future must
/// revoke the worker's pending authority, even after it moved into that worker.
pub(crate) struct StartupWaitGuard {
    lease: Arc<Lease>,
    complete: bool,
}
impl StartupWaitGuard {
    pub(crate) fn complete(&mut self) {
        self.complete = true;
    }
}
impl Drop for StartupWaitGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.lease.revoke();
        }
    }
}

pub(crate) struct AcceptedOffer {
    offer: IncomingOffer,
    lease: Arc<Lease>,
    signal: Arc<Signal>,
    _permit: AcceptPermit,
    handed_off: bool,
}
impl fmt::Debug for AcceptedOffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AcceptedOffer { .. }")
    }
}
impl Drop for AcceptedOffer {
    fn drop(&mut self) {
        if !self.handed_off {
            self.lease.revoke();
        }
    }
}
impl AcceptedOffer {
    pub(crate) fn startup_wait_guard(&self) -> StartupWaitGuard {
        StartupWaitGuard {
            lease: Arc::clone(&self.lease),
            complete: false,
        }
    }
    pub(crate) fn selection(&self) -> &AccountSelection {
        &self.offer.selection
    }
    pub(crate) fn pending_guard(&self) -> PendingGuard {
        PendingGuard {
            lease: Arc::clone(&self.lease),
            signal: Arc::clone(&self.signal),
            selection: self.offer.selection.clone(),
        }
    }
    pub(crate) fn cancelled(&self) -> impl Future<Output = ()> + Send + 'static + use<> {
        let guard = self.pending_guard();
        async move { guard.cancelled().await }
    }
    /// Called under the account-transition lock immediately before join. A
    /// cancelled validation future permanently revokes this accepted offer.
    pub(crate) async fn revalidate_before_join(
        &mut self,
        manager: &AccountManager,
        preferences: &VoiceSpaces,
    ) -> Result<(), ControllerFailure> {
        let mut guard = WorkGuard {
            lease: Arc::clone(&self.lease),
            complete: false,
        };
        let selection = self.offer.selection.clone();
        tokio::select! {
            biased;
            _ = wait_invalid(&self.lease, &self.signal, &selection) => return Err(ControllerFailure::Revoked),
            result = self.offer.revalidate(manager, preferences) => result.map_err(ControllerFailure::Admission)?,
        }
        self.lease.advance(
            &self.signal,
            Instant::now(),
            selection.is_current(),
            Phase::Reserved,
            Phase::Ready,
            self.offer.deadline(),
        )?;
        guard.complete = true;
        Ok(())
    }
    /// Immutable admitted destination for reserving the call worker. This is
    /// not permission to join: begin_join still requires fresh revalidation.
    pub(crate) fn target(&self) -> Result<JoinTarget, ControllerFailure> {
        if !self
            .lease
            .pending(&self.signal, Instant::now(), self.selection().is_current())
        {
            return Err(ControllerFailure::Revoked);
        }
        let options = match self.offer.mode() {
            InteropMode::Current => CallOptions::default(),
            InteropMode::Sticky2025 => CallOptions::element_call_matrix_2_0(),
            InteropMode::RoomState => return Err(ControllerFailure::WrongCall),
        };
        Ok(JoinTarget {
            room_id: self.offer.room.room_id().to_string(),
            options,
            selection: self.offer.selection.clone(),
        })
    }
    pub(crate) fn begin_join(&self) -> Result<JoinTarget, ControllerFailure> {
        let target = self.target()?;
        self.lease.advance(
            &self.signal,
            Instant::now(),
            self.offer.selection.is_current(),
            Phase::Ready,
            Phase::Joining,
            self.offer.deadline(),
        )?;
        Ok(target)
    }
    /// The worker must keep the join future alive for owned cleanup, even when
    /// `cancelled()` resolves. No audio controls/device opening may precede this
    /// successful handoff. On any error the worker explicitly leaves that call.
    pub(crate) async fn commit_join(
        mut self,
        manager: &AccountManager,
        preferences: &VoiceSpaces,
        rtc: &RtcService,
        info: &CallInfo,
    ) -> Result<EstablishedAuthorization, ControllerFailure> {
        let own = JoinedOwn {
            user: self.offer.own_user.clone(),
            device: self.offer.own_device.clone(),
            member_id: info.membership_id.clone(),
        };
        let selection = self.offer.selection.clone();
        // The pinned RTC join acknowledges our send and connects transport; it
        // does not await our sync echo or either independently fed roster.
        // Wait only for absence, never a conflicting identity, and spend the
        // original offer lifetime rather than starting a new startup timeout.
        wait_ready_before_deadline(
            self.offer.deadline(),
            wait_invalid(&self.lease, &self.signal, &selection),
            wait_own_visibility(&mut self.offer, manager, preferences, rtc, info, &own),
        )
        .await?;
        tokio::select! {
            biased;
            _ = wait_invalid(&self.lease, &self.signal, &selection) => return Err(ControllerFailure::Revoked),
            result = self.offer.revalidate_after_join(manager, preferences, &own) => result.map_err(ControllerFailure::Admission)?,
        }
        require_actual_join(&self.offer, rtc, info)?;
        self.lease.advance(
            &self.signal,
            Instant::now(),
            selection.is_current(),
            Phase::Joining,
            Phase::Established,
            self.offer.deadline(),
        )?;
        self.handed_off = true;
        Ok(EstablishedAuthorization {
            guard: EstablishedGuard {
                lease: Arc::clone(&self.lease),
                signal: Arc::clone(&self.signal),
                selection,
                info: info.clone(),
            },
        })
    }
}

pub(super) struct JoinedOwn {
    pub(super) user: String,
    pub(super) device: String,
    pub(super) member_id: String,
}
pub(super) fn own_observation_allowed(
    participants: &[morpheus_rtc::ObservedParticipant],
    user: &str,
    device: &str,
    joined: Option<&JoinedOwn>,
) -> bool {
    let mut own = participants.iter().filter(|member| member.user_id == user);
    match joined {
        None => own.next().is_none(),
        Some(joined) => {
            joined.user == user
                && joined.device == device
                && observed_readiness(participants, joined) == Ok(OwnReadiness::Ready)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OwnReadiness {
    Missing,
    Ready,
}

fn local_readiness(
    participants: &[ParticipantInfo],
    own: &JoinedOwn,
) -> Result<OwnReadiness, ControllerFailure> {
    if participants.len() > super::MAX_LIVE_MEMBERS {
        return Err(ControllerFailure::WrongCall);
    }
    let mut locals = participants.iter().filter(|member| {
        member.is_local || member.user_id == own.user || member.member_id == own.member_id
    });
    let Some(local) = locals.next() else {
        return Ok(OwnReadiness::Missing);
    };
    if locals.next().is_some()
        || !local.is_local
        || local.user_id != own.user
        || local.device_id.as_deref() != Some(own.device.as_str())
        || local.member_id != own.member_id
    {
        return Err(ControllerFailure::WrongCall);
    }
    Ok(OwnReadiness::Ready)
}

fn observed_readiness(
    participants: &[morpheus_rtc::ObservedParticipant],
    own: &JoinedOwn,
) -> Result<OwnReadiness, ControllerFailure> {
    if participants.len() > super::MAX_LIVE_MEMBERS {
        return Err(ControllerFailure::WrongCall);
    }
    let mut locals = participants
        .iter()
        .filter(|member| member.user_id == own.user || member.membership_id == own.member_id);
    let Some(local) = locals.next() else {
        return Ok(OwnReadiness::Missing);
    };
    if locals.next().is_some()
        || local.user_id != own.user
        || local.device_id != own.device
        || local.membership_id != own.member_id
    {
        return Err(ControllerFailure::WrongCall);
    }
    Ok(OwnReadiness::Ready)
}

fn exact_local(participants: &[ParticipantInfo], own: &JoinedOwn) -> bool {
    local_readiness(participants, own) == Ok(OwnReadiness::Ready)
}

async fn wait_ready_before_deadline(
    deadline: Instant,
    cancelled: impl Future<Output = ()>,
    readiness: impl Future<Output = Result<(), ControllerFailure>>,
) -> Result<(), ControllerFailure> {
    tokio::select! {
        biased;
        _ = cancelled => Err(ControllerFailure::Revoked),
        _ = time::sleep_until(time::Instant::from_std(deadline)) => Err(ControllerFailure::Expired),
        result = readiness => result,
    }
}

async fn wait_own_visibility(
    offer: &mut IncomingOffer,
    manager: &AccountManager,
    preferences: &VoiceSpaces,
    rtc: &RtcService,
    info: &CallInfo,
    own: &JoinedOwn,
) -> Result<(), ControllerFailure> {
    require_call_identity(offer, rtc, info)?;
    // Register before reading either current roster. Each snapshot consumes
    // only its own watch version; an update in the inspection gap wakes us.
    let mut direct_changes = rtc
        .call(&info.room_id)
        .ok_or(ControllerFailure::WrongCall)?
        .subscribe_participants();
    loop {
        direct_changes
            .has_changed()
            .map_err(|_| ControllerFailure::Unavailable)?;
        drop(direct_changes.borrow_and_update());
        require_call_identity(offer, rtc, info)?;
        super::require_scope(manager, preferences, &offer.selection, &offer.room)
            .map_err(ControllerFailure::Admission)?;
        offer
            .current_receipt()
            .map_err(ControllerFailure::Admission)?;
        let direct = local_readiness(
            &rtc.participants(&info.room_id)
                .map_err(|_| ControllerFailure::WrongCall)?,
            own,
        )?;
        let observed = offer
            .observer
            .snapshot()
            .await
            .map_err(|_| ControllerFailure::Admission(AdmissionFailure::Membership))?;
        if observed.mode != offer.mode()
            || observed.room_id != info.room_id
            || observed.slot_id != info.slot_id
        {
            return Err(ControllerFailure::WrongCall);
        }
        let passive = observed_readiness(&observed.participants, own)?;
        // A source may disappear or conflict while the fresh slot read awaits.
        // Missing is allowed to wait; any sampled conflict is terminal.
        let fresh_direct = local_readiness(
            &rtc.participants(&info.room_id)
                .map_err(|_| ControllerFailure::WrongCall)?,
            own,
        )?;
        require_call_identity(offer, rtc, info)?;
        super::require_scope(manager, preferences, &offer.selection, &offer.room)
            .map_err(ControllerFailure::Admission)?;
        offer
            .current_receipt()
            .map_err(ControllerFailure::Admission)?;
        if direct == OwnReadiness::Ready
            && fresh_direct == OwnReadiness::Ready
            && passive == OwnReadiness::Ready
        {
            if direct_changes
                .has_changed()
                .map_err(|_| ControllerFailure::Unavailable)?
            {
                continue;
            }
            // Do not accept a closed observer or ignore a change since its
            // snapshot. This poll adds no timer, task, synthetic event or join.
            match offer.observer.changed().now_or_never() {
                Some(Ok(())) => continue,
                Some(Err(_)) => return Err(ControllerFailure::Unavailable),
                None => return Ok(()),
            }
        }
        tokio::select! {
            result = direct_changes.changed() => result.map_err(|_| ControllerFailure::Unavailable)?,
            result = offer.observer.changed() => result.map_err(|_| ControllerFailure::Unavailable)?,
        }
    }
}

fn require_call_identity(
    offer: &IncomingOffer,
    rtc: &RtcService,
    info: &CallInfo,
) -> Result<(), ControllerFailure> {
    if info.room_id != offer.room.room_id().as_str()
        || info.slot_id != "m.call#room"
        || rtc.client().user_id().map(|id| id.as_str()) != Some(offer.own_user.as_str())
        || rtc.client().device_id().map(|id| id.as_str()) != Some(offer.own_device.as_str())
    {
        return Err(ControllerFailure::WrongCall);
    }
    let call = rtc
        .call(&info.room_id)
        .ok_or(ControllerFailure::WrongCall)?;
    if call.membership_id() != info.membership_id || call.local_identity() != info.local_identity {
        return Err(ControllerFailure::WrongCall);
    }
    Ok(())
}
fn require_actual_join(
    offer: &IncomingOffer,
    rtc: &RtcService,
    info: &CallInfo,
) -> Result<(), ControllerFailure> {
    require_call_identity(offer, rtc, info)?;
    let participants = rtc
        .participants(&info.room_id)
        .map_err(|_| ControllerFailure::WrongCall)?;
    if participants.len() > super::MAX_LIVE_MEMBERS
        || !exact_local(
            &participants,
            &JoinedOwn {
                user: offer.own_user.clone(),
                device: offer.own_device.clone(),
                member_id: info.membership_id.clone(),
            },
        )
    {
        return Err(ControllerFailure::WrongCall);
    }
    Ok(())
}

/// Owned by the call worker until leave/cleanup. Cloned read-only guards cannot
/// outlive this owner's authorization, and never re-enable a later call.
pub(crate) struct EstablishedAuthorization {
    guard: EstablishedGuard,
}
impl EstablishedAuthorization {
    pub(crate) fn guard(&self) -> EstablishedGuard {
        self.guard.clone()
    }
}
impl Drop for EstablishedAuthorization {
    fn drop(&mut self) {
        self.guard.lease.revoke();
    }
}
#[derive(Clone)]
pub(crate) struct EstablishedGuard {
    lease: Arc<Lease>,
    signal: Arc<Signal>,
    selection: AccountSelection,
    info: CallInfo,
}
impl EstablishedGuard {
    /// Ring expiry/withdrawal after successful Answer is not a call hangup.
    /// Shutdown, owner drop, and account revocation still stop every waiter.
    pub(crate) async fn cancelled(&self) {
        tokio::select! {
            _ = self.selection.revoked() => {},
            _ = wait_established_invalid(&self.lease, &self.signal) => {},
        }
    }
    pub(crate) fn allows(&self, info: &CallInfo) -> bool {
        &self.info == info
            && self
                .lease
                .established(&self.signal, self.selection.is_current())
    }
}
impl fmt::Debug for EstablishedGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EstablishedGuard { .. }")
    }
}

async fn wait_established_invalid(lease: &Lease, signal: &Signal) {
    loop {
        let global = signal.changed.notified();
        let local = lease.changed.notified();
        tokio::pin!(global);
        tokio::pin!(local);
        global.as_mut().enable();
        local.as_mut().enable();
        if !lease.established(signal, true) {
            return;
        }
        tokio::select! { _ = global => {}, _ = local => {} }
    }
}

async fn wait_invalid(lease: &Lease, signal: &Signal, selection: &AccountSelection) {
    if !selection.is_current() {
        return;
    }
    tokio::select! {
        _ = wait_scope_invalid(lease, signal) => {},
        _ = selection.revoked() => {},
    }
}

async fn wait_scope_invalid(lease: &Lease, signal: &Signal) {
    loop {
        let global = signal.changed.notified();
        let local = lease.changed.notified();
        tokio::pin!(global);
        tokio::pin!(local);
        global.as_mut().enable();
        local.as_mut().enable();
        if !lease.pending(signal, Instant::now(), true) {
            return;
        }
        let Some(deadline) = lease.deadline() else {
            return;
        };
        tokio::select! {
            _ = global => {},
            _ = local => {},
            _ = time::sleep_until(time::Instant::from_std(deadline)) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ledger::Observation;
    use super::*;
    use morpheus_rtc::{ObservedParticipant, ringing::RingWindow};
    use std::time::Duration;

    fn setup() -> (Arc<Signal>, Arc<Lease>, Instant) {
        let signal = Arc::new(Signal::default());
        let now = Instant::now();
        let lease = Arc::new(Lease::new(1, now + Duration::from_secs(30)));
        (signal, lease, now)
    }
    fn advance(
        lease: &Lease,
        signal: &Signal,
        now: Instant,
        from: Phase,
        to: Phase,
    ) -> Result<(), ControllerFailure> {
        lease.advance(signal, now, true, from, to, now + Duration::from_secs(60))
    }

    fn own_decline_info() -> EncryptionInfo {
        use matrix_sdk::ruma::{DeviceKeyAlgorithm, device_id, user_id};
        EncryptionInfo {
            sender: user_id!("@me:example.org").to_owned(),
            sender_device: Some(device_id!("PHONE").to_owned()),
            forwarder: None,
            algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
                curve25519_key: "phone_curve_key".into(),
                sender_claimed_keys: [(DeviceKeyAlgorithm::Ed25519, "phone_signing_key".into())]
                    .into(),
                session_id: Some("phone_session".into()),
            },
            verification_state: VerificationState::Verified,
        }
    }

    /// Only a verified, Megolm-encrypted decline with its own session, sent by
    /// this user from another device and not decrypted with forwarded keys,
    /// may be retained. Plaintext, unverified, forwarded-key, own-device,
    /// foreign or mismatched evidence never qualifies.
    #[test]
    fn only_verified_own_other_device_declines_qualify_at_receipt() {
        use matrix_sdk::{
            deserialized_responses::{ForwarderInfo, VerificationLevel},
            ruma::{device_id, user_id},
        };
        let qualifies = |info: Option<&EncryptionInfo>, sender: &str| {
            decline_receipt_qualifies("@me:example.org", "LAPTOP", sender, info)
        };
        let good = own_decline_info();
        assert!(qualifies(Some(&good), "@me:example.org"));
        assert!(!qualifies(None, "@me:example.org"), "plaintext");
        assert!(
            !qualifies(Some(&good), "@other:example.org"),
            "foreign event sender"
        );
        assert!(!decline_receipt_qualifies(
            "@me:example.org",
            "PHONE",
            "@me:example.org",
            Some(&good)
        ));
        assert!(!decline_receipt_qualifies("", "LAPTOP", "", Some(&good)));

        let mut info = own_decline_info();
        info.verification_state = VerificationState::Unverified(VerificationLevel::UnsignedDevice);
        assert!(!qualifies(Some(&info), "@me:example.org"), "unverified");
        let mut info = own_decline_info();
        info.forwarder = Some(ForwarderInfo {
            user_id: user_id!("@me:example.org").to_owned(),
            device_id: device_id!("TABLET").to_owned(),
        });
        assert!(!qualifies(Some(&info), "@me:example.org"), "forwarded keys");
        let mut info = own_decline_info();
        info.sender = user_id!("@other:example.org").to_owned();
        assert!(
            !qualifies(Some(&info), "@me:example.org"),
            "decrypted sender differs"
        );
        let mut info = own_decline_info();
        info.sender_device = None;
        assert!(!qualifies(Some(&info), "@me:example.org"), "no device");
        let mut info = own_decline_info();
        info.algorithm_info = AlgorithmInfo::MegolmV1AesSha2 {
            curve25519_key: "phone_curve_key".into(),
            sender_claimed_keys: BTreeMap::new(),
            session_id: None,
        };
        assert!(!qualifies(Some(&info), "@me:example.org"), "no session");
        let mut info = own_decline_info();
        info.algorithm_info = AlgorithmInfo::OlmV1Curve25519AesSha2 {
            curve25519_public_key_base64: "phone_curve_key".into(),
        };
        assert!(
            !qualifies(Some(&info), "@me:example.org"),
            "not a room event key"
        );
    }

    struct Fact {
        key: ReceiptKey,
        expires: Instant,
        current: Arc<AtomicBool>,
    }
    impl BufferedReceipt for Fact {
        fn key(&self) -> &ReceiptKey {
            &self.key
        }
        fn expires(&self) -> Instant {
            self.expires
        }
        fn current(&self) -> bool {
            self.current.load(Ordering::SeqCst)
        }
    }
    fn fact(notification: &str, device: &str, expires: Instant, current: &Arc<AtomicBool>) -> Fact {
        Fact {
            key: ReceiptKey {
                account: "account".into(),
                room: "!room:example.org".into(),
                notification: notification.into(),
                device: device.into(),
            },
            expires,
            current: current.clone(),
        }
    }

    /// Retained declines are bounded and short-lived: a duplicate keeps the
    /// first, a full buffer refuses the newest, expired or no longer current
    /// receipts are dropped, and receipts put back after a settlement pass
    /// stay ahead of newer ones.
    #[test]
    fn retained_declines_are_bounded_short_lived_and_scoped() {
        let now = Instant::now();
        let later = now + Duration::from_secs(60);
        let current = Arc::new(AtomicBool::new(true));
        let mut buffer = ReceiptBuffer::default();
        assert!(buffer.insert(fact("$one", "PHONE", later, &current), now));
        assert!(
            !buffer.insert(fact("$one", "PHONE", later, &current), now),
            "duplicate"
        );
        assert!(buffer.insert(fact("$one", "TABLET", later, &current), now));
        assert!(
            !buffer.insert(fact("$past", "PHONE", now, &current), now),
            "expired"
        );
        for index in buffer.len()..MAX_DECLINE_RECEIPTS {
            assert!(buffer.insert(
                fact(&format!("$fill{index}"), "PHONE", later, &current),
                now
            ));
        }
        assert!(
            !buffer.insert(fact("$newest", "PHONE", later, &current), now),
            "full"
        );
        assert_eq!(buffer.len(), MAX_DECLINE_RECEIPTS);

        // Settlement takes everything live; unmatched receipts go back ahead
        // of a newer one, and the bound then drops the newest.
        let taken = buffer.take(now);
        assert_eq!(taken.len(), MAX_DECLINE_RECEIPTS);
        assert_eq!(buffer.len(), 0);
        assert!(buffer.insert(fact("$arrived", "PHONE", later, &current), now));
        buffer.restore(taken, now);
        assert_eq!(buffer.len(), MAX_DECLINE_RECEIPTS);
        assert!(
            buffer
                .take(now)
                .iter()
                .all(|entry| entry.key.notification != "$arrived"),
            "older receipts keep their place"
        );

        // Expiry and a revoked selection drop receipts; clear empties.
        assert!(buffer.insert(
            fact("$short", "PHONE", now + Duration::from_secs(1), &current),
            now
        ));
        assert!(buffer.take(now + Duration::from_secs(1)).is_empty());
        let revoked = Arc::new(AtomicBool::new(true));
        assert!(buffer.insert(fact("$scoped", "PHONE", later, &revoked), now));
        revoked.store(false, Ordering::SeqCst);
        assert!(
            buffer.take(now).is_empty(),
            "a revoked selection's receipt is gone"
        );
        assert!(!buffer.insert(fact("$scoped", "PHONE", later, &revoked), now));
        assert!(buffer.insert(fact("$kept", "PHONE", later, &current), now));
        buffer.clear();
        assert_eq!(buffer.len(), 0);
    }

    struct Scope {
        account: String,
        current: Arc<AtomicBool>,
    }
    impl ReceiptScope for Scope {
        fn scope_account(&self) -> &str {
            &self.account
        }
        fn scope_current(&self) -> bool {
            self.current.load(Ordering::SeqCst)
        }
    }
    fn scope(current: &Arc<AtomicBool>) -> Scope {
        Scope {
            account: "account".into(),
            current: current.clone(),
        }
    }

    /// Retention and its cleanup are one serialized protocol: a callback
    /// dispatched before a stop or a scope change cannot insert afterwards,
    /// receipts taken for settlement cannot come back after a stop or scope
    /// change that happened meanwhile, and only the current scope's own
    /// receipts are ever admitted. Confirming the same current selection
    /// keeps what it already holds.
    #[test]
    fn retained_declines_cannot_outlive_a_stop_or_scope_change() {
        let now = Instant::now();
        let later = now + Duration::from_secs(60);
        let selected = Arc::new(AtomicBool::new(true));
        let mut store: ScopedReceipts<Fact, Scope> = ScopedReceipts::default();
        // Before any scope: nothing is admitted.
        assert!(!store.insert(fact("$early", "PHONE", later, &selected), now));
        store.reset(Some(scope(&selected)));
        assert!(store.insert(fact("$one", "PHONE", later, &selected), now));
        // The same selection confirmed again keeps it.
        store.reset(Some(scope(&selected)));
        assert_eq!(store.len(), 1);
        // Another account's receipt never enters this scope.
        let mut other = fact("$two", "PHONE", later, &selected);
        other.key.account = "other".into();
        assert!(!store.insert(other, now));

        // Taken for settlement, then the scope changes before the restore.
        let taken = store.take(now);
        assert_eq!(taken.len(), 1);
        let newer = Arc::new(AtomicBool::new(true));
        selected.store(false, Ordering::SeqCst);
        store.reset(Some(scope(&newer)));
        store.restore(taken, now);
        assert_eq!(store.len(), 0, "the old scope's receipt is not restored");
        // A late callback from the old selection is refused too.
        assert!(!store.insert(fact("$late", "PHONE", later, &selected), now));
        assert!(store.insert(fact("$new", "PHONE", later, &newer), now));

        // Taken, then stopped before the restore: it stays empty, and a
        // callback dispatched before the stop cannot insert after it.
        let taken = store.take(now);
        store.stop();
        store.restore(taken, now);
        assert_eq!(store.len(), 0);
        assert!(!store.insert(fact("$after", "PHONE", later, &newer), now));
        // Stopped is for good, whatever scope is offered later.
        store.reset(Some(scope(&newer)));
        assert!(!store.insert(fact("$again", "PHONE", later, &newer), now));

        // A scope whose selection was revoked admits nothing.
        let revoked = Arc::new(AtomicBool::new(true));
        let mut store: ScopedReceipts<Fact, Scope> = ScopedReceipts::default();
        store.reset(Some(scope(&revoked)));
        assert!(store.insert(fact("$one", "PHONE", later, &revoked), now));
        revoked.store(false, Ordering::SeqCst);
        assert!(store.take(now).is_empty());
        assert!(!store.insert(fact("$two", "PHONE", later, &revoked), now));
    }

    /// While one offer's decline trust check is held, that offer is withheld
    /// only if it was never shown; another offer stays shown and can still be
    /// taken (Answer, Decline, Dismiss) at once. Taking or draining (scope
    /// change, stop) ends the bookkeeping, and a check that ends afterwards
    /// changes nothing.
    #[test]
    fn a_held_decline_check_withholds_only_its_unshown_offer() {
        let (_, lease, _) = setup();
        let mut book = OfferBook::default();
        let held = book.insert(1u8, lease.clone()).unwrap();
        let other = book.insert(2u8, lease.clone()).unwrap();
        let shown = book.insert(3u8, lease.clone()).unwrap();
        book.entries.get_mut(&shown).unwrap().presented = true;
        assert!(book.begin_check(held, "PHONE"));
        assert!(book.begin_check(shown, "PHONE"));
        assert!(
            book.withheld(held),
            "never shown: withheld while its check runs"
        );
        assert!(!book.withheld(shown), "already shown: keeps ringing");
        assert!(!book.withheld(other));
        // A replay of the same decline while its check is in flight starts
        // nothing.
        assert!(!book.begin_check(held, "PHONE"), "replay while in flight");
        // Another call is answered while the check is held.
        assert_eq!(book.take(other).unwrap().payload, 2);
        assert!(book.withheld(held));
        // The check ends untrusted: the offer is shown normally.
        book.end_check(held);
        assert!(!book.withheld(held));
        // A replay after that failed completion neither checks again nor
        // renews withholding.
        assert!(
            !book.begin_check(held, "PHONE"),
            "replay after a failed check"
        );
        assert!(!book.withheld(held));
        // Two checks for one offer (two devices): withheld until both end.
        assert!(book.begin_check(held, "TABLET"));
        assert!(book.begin_check(held, "DESKTOP"));
        book.end_check(held);
        assert!(book.withheld(held));
        book.end_check(held);
        assert!(!book.withheld(held));
        // Distinct devices per offer are bounded.
        for index in book.entries[&held].checked.len()..MAX_CHECKED_DEVICES {
            assert!(book.begin_check(held, &format!("DEVICE{index}")));
            book.end_check(held);
        }
        assert!(!book.begin_check(held, "ONE_MORE"), "bounded");
        assert!(!book.withheld(held));
        // Expiry or another removal takes it, and a stop drains the rest; a
        // check that ends afterwards, or begins for a gone offer, is inert.
        let fresh = book.insert(4u8, lease.clone()).unwrap();
        assert!(book.begin_check(fresh, "PHONE"));
        book.take(fresh).unwrap();
        book.end_check(fresh);
        assert!(!book.begin_check(fresh, "LAPTOP"));
        assert!(book.begin_check(shown, "TABLET"));
        assert_eq!(book.drain().len(), 2);
        assert!(book.checking.is_empty());
        book.end_check(shown);
        assert!(!book.begin_check(shown, "DESKTOP"));
    }

    /// A decline matches only exactly its offer, and after the trust check
    /// it settles only if every original condition still holds then,
    /// including the receipt's own deadline: a check that completes after the
    /// receipt expired settles nothing, even while the offer stays live.
    #[test]
    fn a_decline_settles_only_its_exact_offer_before_its_own_deadline() {
        let offer = OfferFacts {
            account: "account",
            room: "!room:example.org",
            notification: "$ring",
            own_user: "@me:example.org",
            own_device: "LAPTOP",
        };
        let key = |account: &str, room: &str, notification: &str, device: &str| ReceiptKey {
            account: account.into(),
            room: room.into(),
            notification: notification.into(),
            device: device.into(),
        };
        let exact = key("account", "!room:example.org", "$ring", "PHONE");
        assert!(decline_key_matches(&exact, "@me:example.org", &offer));
        for (wrong, sender) in [
            (
                key("other", "!room:example.org", "$ring", "PHONE"),
                "@me:example.org",
            ),
            (
                key("account", "!other:example.org", "$ring", "PHONE"),
                "@me:example.org",
            ),
            (
                key("account", "!room:example.org", "$older", "PHONE"),
                "@me:example.org",
            ),
            (
                key("account", "!room:example.org", "$ring", "LAPTOP"),
                "@me:example.org",
            ),
            (exact.clone(), "@caller:example.org"),
        ] {
            assert!(
                !decline_key_matches(&wrong, sender, &offer),
                "{wrong:?} {sender}"
            );
        }

        let now = Instant::now();
        let expires = now + Duration::from_secs(1);
        assert!(decline_still_settles(true, true, expires, now, true, true));
        assert!(
            !decline_still_settles(true, true, expires, expires, true, true),
            "trust completed after the receipt's own deadline"
        );
        assert!(!decline_still_settles(
            false, true, expires, now, true, true
        ));
        assert!(!decline_still_settles(
            true, false, expires, now, true, true
        ));
        assert!(!decline_still_settles(
            true, true, expires, now, false, true
        ));
        assert!(!decline_still_settles(
            true, true, expires, now, true, false
        ));
    }
    #[test]
    fn pending_cancellation_does_not_borrow_the_send_only_offer() {
        fn require_send<T: Send>() {}
        fn require_sync<T: Sync>() {}
        fn require_owned<T: Future<Output = ()> + Send + 'static>(_: T) {}
        require_send::<AcceptedOffer>();
        require_send::<PendingGuard>();
        require_sync::<PendingGuard>();
        let _check: fn(AcceptedOffer) = |offer| {
            let waiting = offer.cancelled();
            drop(offer);
            require_owned(waiting);
        };
    }

    /// A Decline owns its offer exclusively: taking it stops the ringing at
    /// once, Answer can no longer reserve or join it, and it stays pending
    /// only while its account, epoch and deadline last. Its cancellation
    /// fence resolves when the scope ends, so an attempt still before its
    /// send reports NotSent rather than sending later.
    #[tokio::test]
    async fn a_decline_claim_excludes_answer_and_ends_with_its_scope() {
        fn require_send<T: Send>() {}
        fn require_send_future<F: Future + Send>(_: &F) {}
        require_send::<DeclineClaim>();
        let _check = |claim: DeclineClaim, manager: &AccountManager, preferences: &VoiceSpaces| {
            let attempt = claim.send(manager, preferences, || true);
            require_send_future(&attempt);
        };

        let (signal, lease, now) = setup();
        advance(&lease, &signal, now, Phase::Offered, Phase::Declining).unwrap();
        assert!(
            !lease.audible.current(true, true, now),
            "declining stops its ringing"
        );
        assert!(lease.pending(&signal, now, true));
        for (from, to) in [
            (Phase::Offered, Phase::Reserved),
            (Phase::Reserved, Phase::Ready),
            (Phase::Ready, Phase::Joining),
            (Phase::Joining, Phase::Established),
        ] {
            assert_eq!(
                advance(&lease, &signal, now, from, to),
                Err(ControllerFailure::Stale),
                "Answer cannot take a declining offer"
            );
        }
        assert!(!lease.established(&signal, true));
        assert!(wait_scope_invalid(&lease, &signal).now_or_never().is_none());
        // Account switch, expiry and invalidation each end it.
        assert!(!lease.pending(&signal, now, false));
        assert!(!lease.pending(&signal, now + Duration::from_secs(30), true));
        InvalidationHandle(signal.clone()).invalidate();
        assert!(!lease.pending(&signal, now, true));
        assert!(wait_scope_invalid(&lease, &signal).now_or_never().is_some());

        // A dropped or finished claim's lease is revoked for good.
        let (signal, lease, now) = setup();
        advance(&lease, &signal, now, Phase::Offered, Phase::Declining).unwrap();
        lease.revoke();
        assert!(!lease.pending(&signal, now, true));
        assert_eq!(
            advance(&lease, &signal, now, Phase::Offered, Phase::Reserved),
            Err(ControllerFailure::Stale)
        );
    }

    #[test]
    fn tokens_are_bounded_single_use_and_never_reused_after_clear() {
        let (signal, lease, _) = setup();
        let mut book = OfferBook::default();
        let tokens: Vec<_> = (0..MAX_OFFERS)
            .map(|i| book.insert(i, lease.clone()).unwrap())
            .collect();
        assert!(matches!(
            book.insert(99, lease.clone()),
            Err(ControllerFailure::Capacity)
        ));
        let first = book.take(tokens[0]).unwrap();
        assert_eq!(first.payload, 0);
        assert!(matches!(
            book.take(tokens[0]),
            Err(ControllerFailure::Stale)
        ));
        let new = book.insert(100, lease).unwrap();
        assert!(!tokens.contains(&new));
        InvalidationHandle(signal).invalidate();
        book.entries.clear();
        assert!(matches!(book.take(new), Err(ControllerFailure::Stale)));
    }
    #[test]
    fn stale_or_double_answer_cannot_advance_to_join() {
        let (signal, lease, now) = setup();
        assert_eq!(
            advance(&lease, &signal, now, Phase::Ready, Phase::Joining),
            Err(ControllerFailure::Stale)
        );
        advance(&lease, &signal, now, Phase::Offered, Phase::Reserved).unwrap();
        assert_eq!(
            advance(&lease, &signal, now, Phase::Offered, Phase::Reserved),
            Err(ControllerFailure::Stale)
        );
        advance(&lease, &signal, now, Phase::Reserved, Phase::Ready).unwrap();
        advance(&lease, &signal, now, Phase::Ready, Phase::Joining).unwrap();
        assert_eq!(
            advance(&lease, &signal, now, Phase::Ready, Phase::Joining),
            Err(ControllerFailure::Stale)
        );
    }
    #[test]
    fn revocation_while_waiting_for_lock_or_join_prevents_commit() {
        for phase in [Phase::Reserved, Phase::Joining] {
            let (signal, lease, now) = setup();
            lease.state.lock().unwrap().phase = phase;
            InvalidationHandle(signal.clone()).invalidate();
            assert!(!lease.pending(&signal, now, true));
            assert_eq!(
                advance(&lease, &signal, now, phase, Phase::Established),
                Err(ControllerFailure::Revoked)
            );
            assert!(!lease.established(&signal, true));
        }
    }
    #[test]
    fn deadline_only_shortens_and_expiry_cannot_commit() {
        let (signal, lease, now) = setup();
        advance(&lease, &signal, now, Phase::Offered, Phase::Reserved).unwrap();
        assert_eq!(lease.deadline(), Some(now + Duration::from_secs(30)));
        lease
            .advance(
                &signal,
                now,
                true,
                Phase::Reserved,
                Phase::Ready,
                now + Duration::from_secs(3),
            )
            .unwrap();
        assert_eq!(lease.deadline(), Some(now + Duration::from_secs(3)));
        assert_eq!(
            advance(
                &lease,
                &signal,
                now + Duration::from_secs(3),
                Phase::Ready,
                Phase::Joining
            ),
            Err(ControllerFailure::Expired)
        );
    }
    #[test]
    fn established_handoff_ignores_ring_withdrawal_but_not_account_shutdown_or_drop() {
        let (signal, lease, now) = setup();
        lease.state.lock().unwrap().phase = Phase::Joining;
        advance(&lease, &signal, now, Phase::Joining, Phase::Established).unwrap();
        assert!(!lease.pending(&signal, now + Duration::from_secs(31), true));
        InvalidationHandle(signal.clone()).invalidate();
        assert!(lease.established(&signal, true));
        assert!(!lease.established(&signal, false));
        InvalidationHandle(signal.clone()).shutdown();
        assert!(!lease.established(&signal, true));
        let (signal, lease, now) = setup();
        lease.state.lock().unwrap().phase = Phase::Joining;
        advance(&lease, &signal, now, Phase::Joining, Phase::Established).unwrap();
        lease.revoke();
        assert!(!lease.established(&signal, true));
    }
    #[test]
    fn cancelled_validation_and_accept_owner_release_are_terminal() {
        let (signal, lease, now) = setup();
        lease.state.lock().unwrap().phase = Phase::Reserved;
        {
            let _work = WorkGuard {
                lease: lease.clone(),
                complete: false,
            };
        }
        assert!(!lease.pending(&signal, now, true));
        let busy = Arc::new(AtomicBool::new(true));
        {
            let _permit = AcceptPermit(busy.clone());
        }
        assert!(!busy.load(Ordering::Acquire));
    }
    #[test]
    fn returning_a_b_a_cannot_revive_the_old_token_or_replay_receipt() {
        let (signal, old, now) = setup();
        let mut ledger = OfferLedger::default();
        let key = OfferKey::new("account-a", "!room:example.org", "$event");
        let window = RingWindow {
            expires_at_ms: 100_000,
            remaining_ms: 30_000,
        };
        assert!(matches!(
            ledger.observe(key, window, now),
            Ok(Observation::FirstSeen { .. })
        ));
        InvalidationHandle(signal.clone()).invalidate(); // A -> B
        InvalidationHandle(signal.clone()).invalidate(); // B -> A (same stable account ID)
        assert!(!old.pending(&signal, now, true));
        assert_eq!(ledger.observe(key, window, now), Ok(Observation::Replay));
        ledger.retire(key).unwrap();
        assert_eq!(ledger.observe(key, window, now), Ok(Observation::Retired));
        let current = Lease::new(
            signal.state.lock().unwrap().epoch,
            now + Duration::from_secs(30),
        );
        assert!(current.pending(&signal, now, true));
    }
    fn own() -> JoinedOwn {
        JoinedOwn {
            user: "@recipient:example.org".into(),
            device: "RECIPIENT".into(),
            member_id: "own-generation".into(),
        }
    }
    fn local() -> ParticipantInfo {
        ParticipantInfo {
            member_id: own().member_id,
            user_id: own().user,
            device_id: Some(own().device),
            is_local: true,
            reachable: true,
            streams: vec![],
            hand_raised_at_ms: None,
            joined_at_ms: None,
        }
    }
    #[test]
    fn actual_own_identity_requires_device_generation_and_local_marker() {
        assert!(exact_local(&[local()], &own()));
        for kind in 0..5 {
            let mut bad = local();
            match kind {
                0 => bad.device_id = None,
                1 => bad.device_id = Some("OTHER".into()),
                2 => bad.member_id = "old-generation".into(),
                3 => bad.is_local = false,
                _ => bad.user_id = "@other:example.org".into(),
            }
            assert!(!exact_local(&[bad], &own()));
        }
        assert!(!exact_local(&[], &own()));
        assert!(!exact_local(&[local(), local()], &own()));
        let mut other_device = local();
        other_device.is_local = false;
        other_device.device_id = Some("OTHER".into());
        assert!(!exact_local(&[local(), other_device], &own()));
    }
    #[test]
    fn post_join_observer_exception_requires_exact_own_membership() {
        let own = own();
        let own_observed = ObservedParticipant {
            user_id: own.user.clone(),
            device_id: own.device.clone(),
            membership_id: own.member_id.clone(),
            membership_event_id: "$own-event".into(),
        };
        assert!(own_observation_allowed(&[], &own.user, &own.device, None));
        assert!(!own_observation_allowed(
            &[own_observed.clone()],
            &own.user,
            &own.device,
            None
        ));
        assert!(own_observation_allowed(
            &[own_observed.clone()],
            &own.user,
            &own.device,
            Some(&own)
        ));
        assert!(!own_observation_allowed(
            &[],
            &own.user,
            &own.device,
            Some(&own)
        ));
        for kind in 0..2 {
            let mut bad = own_observed.clone();
            if kind == 0 {
                bad.device_id = "OTHER".into();
            } else {
                bad.membership_id = "old".into();
            }
            assert!(!own_observation_allowed(
                &[bad],
                &own.user,
                &own.device,
                Some(&own)
            ));
        }
        assert!(!own_observation_allowed(
            &[own_observed.clone(), own_observed],
            &own.user,
            &own.device,
            Some(&own)
        ));
    }
    #[test]
    fn readiness_waits_only_for_absence_and_rejects_conflicting_local_candidates() {
        assert_eq!(local_readiness(&[], &own()), Ok(OwnReadiness::Missing));
        assert_eq!(local_readiness(&[local()], &own()), Ok(OwnReadiness::Ready));
        for kind in 0..6 {
            let mut bad = local();
            match kind {
                0 => bad.device_id = None,
                1 => bad.device_id = Some("OTHER".into()),
                2 => bad.member_id = "previous".into(),
                3 => bad.is_local = false,
                4 => bad.user_id = "@other:example.org".into(),
                _ => {
                    bad.user_id = "@other:example.org".into();
                    bad.is_local = false;
                }
            }
            assert_eq!(
                local_readiness(&[bad], &own()),
                Err(ControllerFailure::WrongCall)
            );
        }
        assert_eq!(
            local_readiness(&[local(), local()], &own()),
            Err(ControllerFailure::WrongCall)
        );
        assert_eq!(
            local_readiness(&vec![local(); super::super::MAX_LIVE_MEMBERS + 1], &own()),
            Err(ControllerFailure::WrongCall)
        );
    }

    #[test]
    fn passive_readiness_never_waits_through_wrong_device_generation_or_duplicate() {
        let identity = own();
        let member = ObservedParticipant {
            user_id: identity.user.clone(),
            device_id: identity.device.clone(),
            membership_id: identity.member_id.clone(),
            membership_event_id: "$actual".into(),
        };
        assert_eq!(
            observed_readiness(&[], &identity),
            Ok(OwnReadiness::Missing)
        );
        assert_eq!(
            observed_readiness(&[member.clone()], &identity),
            Ok(OwnReadiness::Ready)
        );
        for kind in 0..3 {
            let mut bad = member.clone();
            match kind {
                0 => bad.device_id = "OTHER".into(),
                1 => bad.membership_id = "previous".into(),
                _ => bad.user_id = "@other:example.org".into(),
            }
            assert_eq!(
                observed_readiness(&[bad], &identity),
                Err(ControllerFailure::WrongCall)
            );
        }
        assert_eq!(
            observed_readiness(&[member.clone(), member], &identity),
            Err(ControllerFailure::WrongCall)
        );
    }

    #[tokio::test]
    async fn readiness_stays_pending_until_actual_work_completes() {
        let (send, receive) = tokio::sync::oneshot::channel();
        let mut waiting = Box::pin(wait_ready_before_deadline(
            Instant::now() + Duration::from_secs(30),
            std::future::pending(),
            async { receive.await.map_err(|_| ControllerFailure::Unavailable)? },
        ));
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        send.send(Ok(())).unwrap();
        assert_eq!(waiting.await, Ok(()));
    }

    #[tokio::test]
    async fn readiness_revocation_wins_even_when_roster_becomes_ready_together() {
        let (signal, lease, _) = setup();
        lease.state.lock().unwrap().phase = Phase::Joining;
        let (send, receive) = tokio::sync::oneshot::channel();
        let mut waiting = Box::pin(wait_ready_before_deadline(
            lease.deadline().unwrap(),
            wait_scope_invalid(&lease, &signal),
            async { receive.await.map_err(|_| ControllerFailure::Unavailable)? },
        ));
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        InvalidationHandle(signal.clone()).invalidate();
        send.send(Ok(())).unwrap();
        assert_eq!(waiting.await, Err(ControllerFailure::Revoked));
        assert!(!lease.established(&signal, true));
    }

    #[tokio::test]
    async fn readiness_cannot_restart_expired_offer_lifetime() {
        let original = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            wait_ready_before_deadline(original, std::future::pending(), async { Ok(()) }).await,
            Err(ControllerFailure::Expired)
        );
    }

    #[tokio::test]
    async fn readiness_conflict_or_closed_source_is_terminal_without_waiting() {
        for failure in [ControllerFailure::WrongCall, ControllerFailure::Unavailable] {
            assert_eq!(
                wait_ready_before_deadline(
                    Instant::now() + Duration::from_secs(30),
                    std::future::pending(),
                    async { Err(failure) },
                )
                .await,
                Err(failure)
            );
        }
    }

    #[test]
    fn generation_exhaustion_stops_instead_of_reviving_old_offers() {
        let (signal, lease, now) = setup();
        signal.state.lock().unwrap().epoch = u64::MAX;
        InvalidationHandle(signal.clone()).invalidate();
        assert!(signal.state.lock().unwrap().stopped);
        assert!(!lease.pending(&signal, now, true));
        let mut book = OfferBook::default();
        book.next = u64::MAX;
        assert!(matches!(
            book.insert((), lease),
            Err(ControllerFailure::Capacity)
        ));
        assert!(book.entries.is_empty());
    }
    #[tokio::test]
    async fn pending_lock_and_join_waiters_all_wake_on_invalidation() {
        let (signal, lease, _) = setup();
        lease.state.lock().unwrap().phase = Phase::Joining;
        let mut first = Box::pin(wait_scope_invalid(&lease, &signal));
        let mut second = Box::pin(wait_scope_invalid(&lease, &signal));
        let mut abandoned = Box::pin(wait_scope_invalid(&lease, &signal));
        assert!(futures_util::poll!(first.as_mut()).is_pending());
        assert!(futures_util::poll!(second.as_mut()).is_pending());
        assert!(futures_util::poll!(abandoned.as_mut()).is_pending());
        drop(abandoned);
        InvalidationHandle(signal.clone()).invalidate();
        time::timeout(Duration::from_secs(1), first).await.unwrap();
        time::timeout(Duration::from_secs(1), second).await.unwrap();
        // A waiter installed after invalidation cannot lose the notification.
        time::timeout(Duration::from_secs(1), wait_scope_invalid(&lease, &signal))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancelled_revalidation_wakes_retained_startup_waiter() {
        let (signal, lease, _) = setup();
        lease.state.lock().unwrap().phase = Phase::Reserved;
        let mut waiter = Box::pin(wait_scope_invalid(&lease, &signal));
        assert!(futures_util::poll!(waiter.as_mut()).is_pending());
        {
            let _cancelled = WorkGuard {
                lease: lease.clone(),
                complete: false,
            };
        }
        time::timeout(Duration::from_secs(1), waiter).await.unwrap();
    }
    #[test]
    fn ui_keys_distinguish_old_actions_without_exposing_a_token_constructor() {
        assert_ne!(ActionToken(1).ui_key(), ActionToken(2).ui_key());
        assert_eq!(ActionToken(1).ui_key(), ActionToken(1).ui_key());
        assert!(!format!("{:?}", ActionToken(1)).contains("incoming-1"));
    }

    #[tokio::test]
    async fn dropped_startup_wait_revokes_pending_and_just_committed_authority() {
        for phase in [Phase::Reserved, Phase::Joining, Phase::Established] {
            let (signal, lease, now) = setup();
            lease.state.lock().unwrap().phase = phase;
            let mut waiter = Box::pin(wait_established_invalid(&lease, &signal));
            if phase == Phase::Established {
                assert!(futures_util::poll!(waiter.as_mut()).is_pending());
            }
            drop(StartupWaitGuard {
                lease: lease.clone(),
                complete: false,
            });
            assert!(!lease.pending(&signal, now, true));
            assert!(!lease.established(&signal, true));
            time::timeout(Duration::from_secs(1), waiter).await.unwrap();
        }
    }

    #[tokio::test]
    async fn successful_startup_handoff_retains_authority_until_owner_revocation() {
        let (signal, lease, _) = setup();
        lease.state.lock().unwrap().phase = Phase::Established;
        let mut waiting = StartupWaitGuard {
            lease: lease.clone(),
            complete: false,
        };
        waiting.complete();
        drop(waiting);
        let mut established = Box::pin(wait_established_invalid(&lease, &signal));
        assert!(futures_util::poll!(established.as_mut()).is_pending());
        // A delayed ring withdrawal wakes the waiter, but is not a hangup.
        InvalidationHandle(signal.clone()).invalidate();
        assert!(futures_util::poll!(established.as_mut()).is_pending());
        // The call worker's owner is terminal even for already-registered media.
        lease.revoke();
        time::timeout(Duration::from_secs(1), established)
            .await
            .unwrap();
        assert!(!lease.established(&signal, true));
    }

    #[tokio::test]
    async fn shutdown_wakes_all_established_capture_watchers() {
        let (signal, lease, _) = setup();
        lease.state.lock().unwrap().phase = Phase::Established;
        let mut first = Box::pin(wait_established_invalid(&lease, &signal));
        let mut second = Box::pin(wait_established_invalid(&lease, &signal));
        assert!(futures_util::poll!(first.as_mut()).is_pending());
        assert!(futures_util::poll!(second.as_mut()).is_pending());
        InvalidationHandle(signal.clone()).shutdown();
        time::timeout(Duration::from_secs(1), first).await.unwrap();
        time::timeout(Duration::from_secs(1), second).await.unwrap();
    }
    #[test]
    fn audible_claim_is_shared_once_and_stop_cannot_be_reversed() {
        let now = Instant::now();
        let sound = Arc::new(AudibleState::new(now + Duration::from_secs(30)));
        assert!(sound.claim());
        assert!(!sound.clone().claim());
        assert!(sound.current(true, true, now));
        sound.stop();
        assert!(!sound.current(true, true, now));
        assert!(!sound.claim());
    }
    #[test]
    fn audible_expiry_selection_and_observed_epoch_are_independent_vetoes() {
        let now = Instant::now();
        let sound = AudibleState::new(now + Duration::from_secs(1));
        assert!(!sound.current(false, true, now));
        assert!(!sound.current(true, false, now));
        assert!(!sound.current(true, true, now + Duration::from_secs(1)));
        let signal = Arc::new(Signal::default());
        let handle = InvalidationHandle(signal.clone());
        let before = signal.audio_epoch.load(Ordering::Acquire);
        handle.invalidate();
        handle.invalidate();
        assert_ne!(signal.audio_epoch.load(Ordering::Acquire), before);
        handle.shutdown();
        assert_eq!(signal.audio_epoch.load(Ordering::Acquire), 0);
    }
    #[test]
    fn answer_reservation_and_dismiss_revoke_audio_before_network_work() {
        let (signal, lease, now) = setup();
        assert!(lease.audible.current(true, true, now));
        lease
            .advance(
                &signal,
                now,
                true,
                Phase::Offered,
                Phase::Reserved,
                now + Duration::from_secs(30),
            )
            .unwrap();
        assert!(!lease.audible.current(true, true, now));
        let other = Lease::new(1, now + Duration::from_secs(30));
        other.revoke();
        assert!(!other.audible.current(true, true, now));
    }
}
