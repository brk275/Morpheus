//! Bounded offer ownership and single-use Answer authorization.
//!
//! No discovery task, UI callback, slot write or media operation lives here.
//! The bounded worker supplies retained sync event IDs and cached change hints;
//! policy publication synchronously invalidates pending authority. SDK change
//! streams invalidate when observed, and Answer/postjoin use full revalidation.
//! Account switches retain this controller/ledger. Private integration is source
//! reviewed; real controller, watcher and media acceptance is still pending.

use super::{
    AdmissionFailure, IncomingOffer,
    ledger::{OfferKey, OfferLedger},
};
use crate::voice_spaces::VoiceSpaces;
use futures_util::FutureExt;
use matrix_sdk::ruma::{EventId, RoomId};
use morpheus_matrix::{AccountManager, AccountSelection};
use morpheus_rtc::{CallInfo, CallOptions, InteropMode, ParticipantInfo, RtcService};
use std::{
    collections::BTreeMap,
    fmt,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use tokio::{sync::Notify, time};

const MAX_OFFERS: usize = 4;

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
}
impl fmt::Debug for OfferPresentation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OfferPresentation { .. }")
    }
}

struct SignalState {
    epoch: u64,
    stopped: bool,
}
struct Signal {
    state: Mutex<SignalState>,
    changed: Notify,
}
impl Default for Signal {
    fn default() -> Self {
        Self {
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
        drop(state);
        self.0.changed.notify_waiters();
    }
    pub(crate) fn shutdown(&self) {
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
    Revoked,
}
struct LeaseState {
    phase: Phase,
    deadline: Instant,
}
struct Lease {
    epoch: u64,
    state: Mutex<LeaseState>,
    changed: Notify,
}
impl Lease {
    fn new(epoch: u64, deadline: Instant) -> Self {
        Self {
            epoch,
            state: Mutex::new(LeaseState {
                phase: Phase::Offered,
                deadline,
            }),
            changed: Notify::new(),
        }
    }
    fn revoke(&self) {
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
                Phase::Offered | Phase::Reserved | Phase::Ready | Phase::Joining
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

struct Pending<T> {
    payload: T,
    lease: Arc<Lease>,
}
struct OfferBook<T> {
    entries: BTreeMap<ActionToken, Pending<T>>,
    next: u64,
}
impl<T> Default for OfferBook<T> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            next: 1,
        }
    }
}
impl<T> OfferBook<T> {
    fn insert(&mut self, payload: T, lease: Arc<Lease>) -> Result<ActionToken, ControllerFailure> {
        if self.entries.len() >= MAX_OFFERS {
            return Err(ControllerFailure::Capacity);
        }
        let token = ActionToken(self.next);
        self.next = self
            .next
            .checked_add(1)
            .ok_or(ControllerFailure::Capacity)?;
        self.entries.insert(token, Pending { payload, lease });
        Ok(token)
    }
    fn take(&mut self, token: ActionToken) -> Result<Pending<T>, ControllerFailure> {
        self.entries.remove(&token).ok_or(ControllerFailure::Stale)
    }
}

pub(crate) struct IncomingController {
    ledger: OfferLedger,
    book: OfferBook<IncomingOffer>,
    signal: Arc<Signal>,
    selected: Option<AccountSelection>,
    accepting: Arc<AtomicBool>,
}
impl Default for IncomingController {
    fn default() -> Self {
        Self {
            ledger: OfferLedger::default(),
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
        let _ = self.ledger.retire(OfferKey::new(
            offer.selection.account_id(),
            offer.room.room_id().as_str(),
            offer.ring.event_id(),
        ));
    }
    fn clear_offers(&mut self) {
        let entries = std::mem::take(&mut self.book.entries);
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
    pub(crate) async fn receive(
        &mut self,
        manager: &AccountManager,
        preferences: &VoiceSpaces,
        room: &RoomId,
        event: &EventId,
    ) -> Result<ActionToken, ControllerFailure> {
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
                .retire(OfferKey::new(
                    selection.account_id(),
                    room.as_str(),
                    event.as_str(),
                ))
                .map_err(|_| ControllerFailure::Capacity)?;
            return Err(ControllerFailure::Capacity);
        }
        let offer = IncomingOffer::admit(manager, preferences, room, event, &mut self.ledger)
            .await
            .map_err(ControllerFailure::Admission)?;
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
        self.book
            .entries
            .iter()
            .map(|(token, pending)| OfferPresentation {
                token: *token,
                account_id: pending.payload.selection.account_id().to_owned(),
                room_id: pending.payload.room.room_id().to_string(),
                caller_user_id: pending.payload.ring.sender().to_owned(),
                mode: pending.payload.mode(),
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
}
