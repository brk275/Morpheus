//! Own-device verification bound to one exact account selection.
//!
//! The account's other devices ask this device to verify (or this device
//! asks them); each request is a flow identified by its flow ID. This module
//! lists the incoming requests the selected account's client has recorded,
//! bounded to `MAX_PENDING_VERIFICATIONS`, with the other device's ID and
//! its own display name when the SDK has them, and runs every step of one
//! flow for exactly that flow:
//!
//! - accept a request (offering only `m.sas.v1`),
//! - start or accept the SAS comparison,
//! - read the SAS values, which the person must compare out of band,
//! - confirm a match the person explicitly reported, or cancel.
//!
//! Cryptography, trust and the protocol are the pinned SDK's: nothing here
//! verifies automatically, marks a device trusted, or adds a method. Only
//! self-verification flows of the selected account are reachable; a flow
//! of another user is `Gone`.
//!
//! Every call first takes the client under the manager's state lock and
//! releases it before any await on the SDK, so an abandoned request never
//! blocks account navigation. Each wait is raced against the caller's
//! `VerificationCancellation` and the selection's revocation, and bounded.
//! A step that sends an event checks the view, the selection and the
//! caller's admission fence (the app's security-gate generation)
//! immediately before it is handed to the SDK; nothing is sent if any
//! fails. Once handed over, any stop or error is `Unknown`: the event may
//! have reached the other device, and nothing is resent. Error text is fixed.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use matrix_sdk::{
    Client,
    encryption::verification::{SasVerification, VerificationRequest, VerificationRequestState},
    ruma::{
        OwnedUserId,
        events::key::verification::{VerificationMethod, cancel::CancelCode},
    },
};
use tokio::{sync::Notify, time};

use crate::{
    AccountManager, AccountSelection, LoadedAccount, SasEmojiInfo, SasPresentation, account_access,
    selected_loaded_for,
};

/// Incoming requests listed at once, newest first.
pub const MAX_PENDING_VERIFICATIONS: usize = 16;
/// Longest device display name kept, in characters.
pub const MAX_VERIFICATION_DEVICE_NAME_CHARS: usize = 64;
/// Matrix verification requests expire ten minutes after they are sent.
pub const VERIFICATION_REQUEST_LIFETIME: Duration = Duration::from_secs(10 * 60);
// Reads of the client and its crypto store, no network.
pub(crate) const LOCAL_TIMEOUT: Duration = Duration::from_secs(10);
// One step sent to the other device.
pub(crate) const SEND_TIMEOUT: Duration = Duration::from_secs(30);
// The own identity may be fetched from the homeserver before a request.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
/// Incoming flow IDs recorded per account: the event handler drops the
/// oldest beyond this, and a listing examines at most this many.
pub(crate) const MAX_RECORDED_VERIFICATIONS: usize = 64;

/// Why a verification step did not complete. The text is fixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OwnVerificationError {
    #[error("This verification view is closed")]
    Cancelled,
    #[error("The selected account changed")]
    AccountChanged,
    #[error("Finish the account or security step in progress, then try again.")]
    SecurityBusy,
    #[error("This request is no longer available. It may have expired or been answered elsewhere.")]
    Gone,
    #[error("The other device has not reached this step yet. Keep both devices open.")]
    NotReady,
    #[error("The other device does not offer emoji or number comparison.")]
    Unsupported,
    #[error("Verification is not available for this account yet. Try again in a moment.")]
    Unavailable,
    #[error("No answer in time. Check the status before trying again.")]
    TimedOut,
    #[error(
        "This step may or may not have reached the other device. Check the status; it was not sent again."
    )]
    Unknown,
    #[error(
        "An earlier request may have been sent. Check your other devices before sending another."
    )]
    RequestUncertain,
    #[error("A verification request is already being sent. Wait for it to finish.")]
    RequestInProgress,
}

pub type OwnVerificationResult<T> = std::result::Result<T, OwnVerificationError>;

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

/// Ends one verification view and everything it waits for. A step already
/// handed to the SDK may still reach the other device.
#[derive(Clone, Default)]
pub struct VerificationCancellation(Arc<CancellationState>);

impl VerificationCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    /// Resolves once any clone is cancelled.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// Where one flow stands, as the SDK reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnVerificationPhase {
    /// Sent by this device, not answered yet.
    Created,
    /// Received from another device, not answered here yet.
    Requested,
    /// Both sides accepted the request; a comparison can start.
    Ready,
    /// The SAS comparison is under way.
    Comparing,
    Done,
    /// Cancelled by either side, other than by expiry.
    Cancelled {
        by_us: bool,
    },
    /// The request expired before it was completed.
    Expired,
}

/// One flow of the selected account. Labels are the other device's own,
/// bounded and unverified: a device names itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnVerification {
    pub flow_id: String,
    pub own_user_id: String,
    /// The other device, once the SDK knows which one it is.
    pub device_id: Option<String>,
    pub device_name: Option<String>,
    pub we_started: bool,
    pub phase: OwnVerificationPhase,
    /// Whether the other device offers SAS; `None` while not known.
    pub offers_sas: Option<bool>,
    /// When the requesting device says it sent this request (its own clock,
    /// ms since the Unix epoch); `None` when not known.
    pub sent_at_ms: Option<u64>,
}

/// What a request for verification of this device did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnVerificationRequest {
    /// A new request was sent.
    Sent(OwnVerification),
    /// A request this device sent earlier is still open: it is reused and
    /// nothing new is sent.
    AlreadyOpen(OwnVerification),
}

/// Requests this device sent, per loaded account, at most this many.
const MAX_OUTGOING_FLOWS: usize = 8;

/// One loaded account's own-device verification bookkeeping. It lives in
/// that account's `LoadedAccount` (created and dropped with it), so it is
/// bounded by the loaded accounts, and a request still finishing after a
/// sign-out or reload can only settle into the book it started on, never
/// into a newer one.
#[derive(Debug, Default)]
pub(crate) struct VerificationBook(std::sync::Mutex<Book>);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Book {
    /// Requests this device sent (newest last, bounded): the SDK has no
    /// public listing of them, and an open one is reused.
    outgoing: Vec<String>,
    /// The last request may have been sent without its ID coming back.
    uncertain: bool,
    /// A request is being prepared or sent right now.
    in_flight: bool,
    /// The incoming record dropped a request at its bound at least once.
    incoming_overflowed: bool,
}

impl VerificationBook {
    fn with<R>(&self, f: impl FnOnce(&mut Book) -> R) -> R {
        let mut book = self.0.lock().unwrap_or_else(|error| error.into_inner());
        f(&mut book)
    }

    /// The incoming record dropped a request: remembered for the life of
    /// this loaded account, whatever later prunes do.
    pub(crate) fn mark_incoming_overflow(&self) {
        self.with(|book| book.incoming_overflowed = true);
    }

    fn incoming_overflowed(&self) -> bool {
        self.with(|book| book.incoming_overflowed)
    }
}

/// The one request of a loaded account that may be in flight, reserved
/// before any lookup, identity read or send. It is settled by the core,
/// whatever the view does: a sent request is remembered, an unknown one
/// blocks the next until acknowledged, and an unsent one changes nothing.
/// Dropped unsettled (its future dropped), it counts as unknown once it was
/// handed to the SDK and as unsent before.
struct Reservation {
    book: Arc<VerificationBook>,
    handed_off: bool,
    settled: bool,
}

impl Reservation {
    /// Reserve, refusing while another request is in flight or while the
    /// last one is uncertain and not acknowledged. Returns the remembered
    /// sent flows, newest last.
    fn reserve(
        book: &Arc<VerificationBook>,
        acknowledged: bool,
    ) -> OwnVerificationResult<(Self, Vec<String>)> {
        let flows = book.with(|state| {
            if state.in_flight {
                return Err(OwnVerificationError::RequestInProgress);
            }
            if state.uncertain && !acknowledged {
                return Err(OwnVerificationError::RequestUncertain);
            }
            state.in_flight = true;
            Ok(state.outgoing.clone())
        })?;
        let reservation = Self {
            book: book.clone(),
            handed_off: false,
            settled: false,
        };
        Ok((reservation, flows))
    }

    fn hand_off(&mut self) {
        self.handed_off = true;
    }

    fn settle(&mut self, f: impl FnOnce(&mut Book)) {
        self.book.with(|book| {
            f(book);
            book.in_flight = false;
        });
        self.settled = true;
    }

    fn sent(&mut self, flow_id: &str) {
        self.settle(|book| record_outgoing(book, flow_id));
    }

    fn unknown(&mut self) {
        self.settle(|book| book.uncertain = true);
    }

    fn unsent(&mut self) {
        self.settle(|_| {});
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.settled {
            let handed_off = self.handed_off;
            self.book.with(|book| {
                if handed_off {
                    book.uncertain = true;
                }
                book.in_flight = false;
            });
        }
    }
}

/// A request was sent: remember its flow (newest last, bounded).
fn record_outgoing(book: &mut Book, flow_id: &str) {
    book.uncertain = false;
    book.outgoing.retain(|flow| flow != flow_id);
    book.outgoing.push(flow_id.to_owned());
    let excess = book.outgoing.len().saturating_sub(MAX_OUTGOING_FLOWS);
    book.outgoing.drain(..excess);
}

/// One bounded listing of incoming requests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PendingOwnVerifications {
    /// Open requests, newest first, at most `MAX_PENDING_VERIFICATIONS`.
    pub requests: Vec<OwnVerification>,
    /// Further open requests found but not listed.
    pub more_open: usize,
    /// The recorded list was full: older requests may have been dropped
    /// before this listing and are not counted.
    pub older_may_be_omitted: bool,
}

/// Record one incoming flow, keeping at most `MAX_RECORDED_VERIFICATIONS`
/// (the oldest go first). A flow already recorded is not added again.
/// `true` when an older flow was dropped to stay within the bound.
pub(crate) fn record_incoming(
    recorded: &mut Vec<crate::VerificationInfo>,
    info: crate::VerificationInfo,
) -> bool {
    if recorded.iter().any(|known| known.flow_id == info.flow_id) {
        return false;
    }
    recorded.push(info);
    let excess = recorded.len().saturating_sub(MAX_RECORDED_VERIFICATIONS);
    recorded.drain(..excess);
    excess > 0
}

/// The newest recorded flow IDs with their sent times, at most
/// `MAX_RECORDED_VERIFICATIONS`, copied without cloning the rest; and
/// whether the list was full.
fn newest_recorded(recorded: &[crate::VerificationInfo]) -> (Vec<(String, Option<u64>)>, bool) {
    let candidates = recorded
        .iter()
        .rev()
        .take(MAX_RECORDED_VERIFICATIONS)
        .map(|known| (known.flow_id.clone(), known.sent_at_ms))
        .collect();
    (candidates, recorded.len() >= MAX_RECORDED_VERIFICATIONS)
}

/// Split the open requests into the listed ones and a count of the rest.
fn bounded_listing(
    open: Vec<OwnVerification>,
    older_may_be_omitted: bool,
) -> PendingOwnVerifications {
    let more_open = open.len().saturating_sub(MAX_PENDING_VERIFICATIONS);
    let requests = open.into_iter().take(MAX_PENDING_VERIFICATIONS).collect();
    PendingOwnVerifications {
        requests,
        more_open,
        older_may_be_omitted,
    }
}

impl OwnVerification {
    pub fn is_open(&self) -> bool {
        !matches!(
            self.phase,
            OwnVerificationPhase::Done
                | OwnVerificationPhase::Cancelled { .. }
                | OwnVerificationPhase::Expired
        )
    }
}

fn bounded_name(name: Option<&str>) -> Option<String> {
    let name: String = name?
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_VERIFICATION_DEVICE_NAME_CHARS)
        .collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

fn offers_sas(methods: &[VerificationMethod]) -> bool {
    methods.contains(&VerificationMethod::SasV1)
}

/// The listing of one request, from the SDK's current state.
pub(crate) fn snapshot(request: &VerificationRequest) -> OwnVerification {
    let mut info = OwnVerification {
        flow_id: request.flow_id().to_owned(),
        own_user_id: request.own_user_id().to_string(),
        device_id: None,
        device_name: None,
        we_started: request.we_started(),
        phase: OwnVerificationPhase::Created,
        offers_sas: None,
        sent_at_ms: None,
    };
    match request.state() {
        VerificationRequestState::Created { .. } => {}
        VerificationRequestState::Requested {
            their_methods,
            other_device_data,
        } => {
            info.phase = OwnVerificationPhase::Requested;
            info.offers_sas = Some(offers_sas(&their_methods));
            info.device_id = Some(other_device_data.device_id().to_string());
            info.device_name = bounded_name(other_device_data.display_name());
        }
        VerificationRequestState::Ready {
            their_methods,
            other_device_data,
            ..
        } => {
            info.phase = OwnVerificationPhase::Ready;
            info.offers_sas = Some(offers_sas(&their_methods));
            info.device_id = Some(other_device_data.device_id().to_string());
            info.device_name = bounded_name(other_device_data.display_name());
        }
        VerificationRequestState::Transitioned { verification } => {
            info.phase = OwnVerificationPhase::Comparing;
            if let Some(sas) = verification.sas() {
                info.offers_sas = Some(true);
                let device = sas.other_device();
                info.device_id = Some(device.device_id().to_string());
                info.device_name = bounded_name(device.display_name());
            } else {
                // Another method (QR) took over: not one Morpheus supports.
                info.offers_sas = Some(false);
            }
        }
        VerificationRequestState::Done => info.phase = OwnVerificationPhase::Done,
        VerificationRequestState::Cancelled(cancel) => {
            info.phase = if cancel.cancel_code() == &CancelCode::Timeout {
                OwnVerificationPhase::Expired
            } else {
                OwnVerificationPhase::Cancelled {
                    by_us: cancel.cancelled_by_us(),
                }
            };
        }
    }
    info
}

pub(crate) fn scope_error(
    selection: &AccountSelection,
    cancellation: &VerificationCancellation,
) -> OwnVerificationResult<()> {
    if cancellation.is_cancelled() {
        return Err(OwnVerificationError::Cancelled);
    }
    if !selection.is_current() {
        return Err(OwnVerificationError::AccountChanged);
    }
    Ok(())
}

/// One wait that sends nothing: it ends at once on cancellation, account
/// change or timeout, and is re-checked before its result is used.
pub(crate) async fn guarded<T>(
    selection: &AccountSelection,
    cancellation: &VerificationCancellation,
    limit: Duration,
    work: impl Future<Output = OwnVerificationResult<T>>,
) -> OwnVerificationResult<T> {
    scope_error(selection, cancellation)?;
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(OwnVerificationError::Cancelled),
        () = selection.revoked() => Err(OwnVerificationError::AccountChanged),
        result = time::timeout(limit, work) => {
            result.unwrap_or(Err(OwnVerificationError::TimedOut))
        }
    }?;
    scope_error(selection, cancellation)?;
    Ok(result)
}

/// One step handed to the SDK, which sends it to the other device. The
/// view, the selection and `admitted` are checked immediately before; from
/// the hand-off on, every stop or failure is Unknown and nothing is resent.
/// Any SDK error type is accepted (`matrix_sdk::Error` for most steps,
/// `RequestVerificationError` for a new request): the SDK may have sent the
/// event before failing, so each maps to Unknown without being inspected.
pub(crate) async fn dispatch<T, E>(
    selection: &AccountSelection,
    cancellation: &VerificationCancellation,
    admitted: &(dyn Fn() -> bool + Send + Sync),
    limit: Duration,
    work: impl Future<Output = std::result::Result<T, E>>,
) -> OwnVerificationResult<T> {
    scope_error(selection, cancellation)?;
    if !admitted() {
        return Err(OwnVerificationError::SecurityBusy);
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(OwnVerificationError::Unknown),
        () = selection.revoked() => Err(OwnVerificationError::Unknown),
        sent = time::timeout(limit, work) => match sent {
            Ok(Ok(value)) => Ok(value),
            _ => Err(OwnVerificationError::Unknown),
        }
    }
}

/// The selected account's client and own user, taken under the manager's
/// state lock and released before anything else is awaited.
pub(crate) struct Scoped {
    pub(crate) loaded: Arc<LoadedAccount>,
    pub(crate) own: OwnedUserId,
}

impl AccountManager {
    pub(crate) async fn verification_scope(
        &self,
        selection: &AccountSelection,
        cancellation: &VerificationCancellation,
    ) -> OwnVerificationResult<Scoped> {
        let loaded = guarded(selection, cancellation, LOCAL_TIMEOUT, async {
            let state = self.state.lock().await;
            let current = self.account_selection(selection.account_id());
            let current = current.ok_or(OwnVerificationError::AccountChanged)?;
            if !selection.same_epoch(&current) {
                return Err(OwnVerificationError::AccountChanged);
            }
            account_access::ensure_eligible(&state, selection.account_id())
                .map_err(|_| OwnVerificationError::AccountChanged)?;
            let loaded = selected_loaded_for(&state, selection.account_id())
                .map_err(|_| OwnVerificationError::AccountChanged)?;
            Ok(loaded.clone())
        })
        .await?;
        let own = loaded
            .client
            .user_id()
            .ok_or(OwnVerificationError::Unavailable)?;
        let own = own.to_owned();
        Ok(Scoped { loaded, own })
    }

    /// The one self-verification request `flow_id` of the selected account.
    async fn own_request(
        &self,
        selection: &AccountSelection,
        cancellation: &VerificationCancellation,
        flow_id: &str,
    ) -> OwnVerificationResult<(Client, VerificationRequest)> {
        let scoped = self.verification_scope(selection, cancellation).await?;
        let client = scoped.loaded.client.clone();
        let request = guarded(selection, cancellation, LOCAL_TIMEOUT, async {
            let encryption = client.encryption();
            let request = encryption
                .get_verification_request(&scoped.own, flow_id)
                .await;
            request.ok_or(OwnVerificationError::Gone)
        })
        .await?;
        if !request.is_self_verification() || request.other_user_id() != scoped.own {
            return Err(OwnVerificationError::Gone);
        }
        Ok((client, request))
    }

    /// The SAS flow of one request, if it has started.
    async fn own_sas(
        &self,
        selection: &AccountSelection,
        cancellation: &VerificationCancellation,
        client: &Client,
        request: &VerificationRequest,
    ) -> OwnVerificationResult<Option<SasVerification>> {
        let own = request.own_user_id().to_owned();
        let flow_id = request.flow_id().to_owned();
        guarded(selection, cancellation, LOCAL_TIMEOUT, async {
            let verification = client.encryption().get_verification(&own, &flow_id).await;
            Ok(verification.and_then(|verification| verification.sas()))
        })
        .await
    }

    /// Incoming requests from the selected account's other devices that
    /// are still open, newest first, at most `MAX_PENDING_VERIFICATIONS`,
    /// with a count of the further open ones. At most
    /// `MAX_RECORDED_VERIFICATIONS` recorded flows are copied and examined.
    /// Finished and cancelled flows are dropped from the recorded list.
    pub async fn pending_own_verifications_selected(
        &self,
        selection: &AccountSelection,
        cancellation: &VerificationCancellation,
    ) -> OwnVerificationResult<PendingOwnVerifications> {
        let scoped = self.verification_scope(selection, cancellation).await?;
        let (candidates, full) = newest_recorded(&scoped.loaded.incoming_verification_tx.borrow());
        let client = scoped.loaded.client.clone();
        let own = scoped.own.clone();
        let open = guarded(selection, cancellation, LOCAL_TIMEOUT, async {
            let encryption = client.encryption();
            let mut open = Vec::new();
            for (flow_id, sent_at_ms) in &candidates {
                let Some(request) = encryption.get_verification_request(&own, flow_id).await else {
                    continue;
                };
                if !request.is_self_verification() || request.we_started() {
                    continue;
                }
                let mut info = snapshot(&request);
                info.sent_at_ms = *sent_at_ms;
                if info.is_open() {
                    open.push(info);
                }
            }
            Ok(open)
        })
        .await?;
        // Drop the examined flows that are no longer open; one recorded
        // while this read ran is kept. Only the newest flows are remembered.
        scoped
            .loaded
            .incoming_verification_tx
            .send_modify(|recorded| {
                recorded.retain(|candidate| {
                    !candidates
                        .iter()
                        .any(|(flow, _)| *flow == candidate.flow_id)
                        || open.iter().any(|info| info.flow_id == candidate.flow_id)
                });
                let excess = recorded.len().saturating_sub(MAX_RECORDED_VERIFICATIONS);
                recorded.drain(..excess);
                if excess > 0 {
                    scoped.loaded.verification_book.mark_incoming_overflow();
                }
            });
        // Once anything was dropped for this loaded account, the listing
        // keeps saying so, even after prunes shrink the record again.
        let book = &scoped.loaded.verification_book;
        let older_may_be_omitted = full || book.incoming_overflowed();
        Ok(bounded_listing(open, older_may_be_omitted))
    }

    /// Where one flow stands now. Local: no request is sent.
    pub async fn own_verification_selected(
        &self,
        selection: &AccountSelection,
        cancellation: &VerificationCancellation,
        flow_id: &str,
    ) -> OwnVerificationResult<OwnVerification> {
        let (_, request) = self.own_request(selection, cancellation, flow_id).await?;
        Ok(snapshot(&request))
    }

    /// Ask the account's other devices to verify this one, offering SAS
    /// only. One request; an already-open request is not replaced.
    ///
    /// A request this device sent earlier that is still open is reused and
    /// nothing is sent. After an attempt whose outcome is unknown (it may
    /// have sent a request whose ID never came back), no new request starts
    /// until the caller passes `acknowledged_uncertain`, after the person
    /// checked their other devices.
    pub async fn request_own_verification_selected(
        &self,
        selection: &AccountSelection,
        cancellation: &VerificationCancellation,
        admitted: &(dyn Fn() -> bool + Send + Sync),
        acknowledged_uncertain: bool,
    ) -> OwnVerificationResult<OwnVerificationRequest> {
        let scoped = self.verification_scope(selection, cancellation).await?;
        let client = scoped.loaded.client.clone();
        let own = scoped.own.clone();
        // Reserved before any lookup, identity read or send, and settled
        // here whatever the view does (see `Reservation`). Only taking the
        // client under the manager's state lock came before.
        let (mut reservation, flows) =
            Reservation::reserve(&scoped.loaded.verification_book, acknowledged_uncertain)?;
        // An open request this device sent is reused, newest first.
        let open = guarded(selection, cancellation, LOCAL_TIMEOUT, async {
            let encryption = client.encryption();
            for flow_id in flows.iter().rev() {
                if let Some(request) = encryption.get_verification_request(&own, flow_id).await
                    && request.we_started()
                    && request.is_self_verification()
                {
                    let info = snapshot(&request);
                    if info.is_open() {
                        return Ok(Some(info));
                    }
                }
            }
            Ok(None)
        })
        .await?;
        if let Some(info) = open {
            reservation.unsent();
            return Ok(OwnVerificationRequest::AlreadyOpen(info));
        }
        let identity = guarded(selection, cancellation, REQUEST_TIMEOUT, async {
            let identity = client.encryption().get_user_identity(&own).await;
            let identity = identity.map_err(|_| OwnVerificationError::Unavailable)?;
            identity.ok_or(OwnVerificationError::Unavailable)
        })
        .await?;
        let sent = identity.request_verification_with_methods(vec![VerificationMethod::SasV1]);
        reservation.hand_off();
        match dispatch(selection, cancellation, admitted, SEND_TIMEOUT, sent).await {
            Ok(request) => {
                let info = snapshot(&request);
                reservation.sent(&info.flow_id);
                Ok(OwnVerificationRequest::Sent(info))
            }
            Err(OwnVerificationError::Unknown) => {
                reservation.unknown();
                Err(OwnVerificationError::Unknown)
            }
            // Refused before the hand-off (view, selection, fence): unsent.
            Err(error) => {
                reservation.unsent();
                Err(error)
            }
        }
    }

    /// Accept one incoming request from another device of this account,
    /// offering SAS only. Only a request still waiting for an answer here,
    /// whose device offers SAS, is accepted.
    pub async fn accept_own_verification_selected(
        &self,
        selection: &AccountSelection,
        cancellation: &VerificationCancellation,
        flow_id: &str,
        admitted: &(dyn Fn() -> bool + Send + Sync),
    ) -> OwnVerificationResult<OwnVerification> {
        let (_, request) = self.own_request(selection, cancellation, flow_id).await?;
        let info = snapshot(&request);
        if info.we_started || !info.is_open() {
            return Err(OwnVerificationError::Gone);
        }
        if info.phase != OwnVerificationPhase::Requested {
            return Err(OwnVerificationError::NotReady);
        }
        if info.offers_sas != Some(true) {
            return Err(OwnVerificationError::Unsupported);
        }
        let sent = request.accept_with_methods(vec![VerificationMethod::SasV1]);
        dispatch(selection, cancellation, admitted, SEND_TIMEOUT, sent).await?;
        Ok(snapshot(&request))
    }

    /// Start the SAS comparison of a request this device sent, once the
    /// other device accepted it. A comparison already started is kept: a
    /// repeated call never sends a competing start.
    pub async fn start_own_sas_selected(
        &self,
        selection: &AccountSelection,
        cancellation: &VerificationCancellation,
        flow_id: &str,
        admitted: &(dyn Fn() -> bool + Send + Sync),
    ) -> OwnVerificationResult<()> {
        let (client, request) = self.own_request(selection, cancellation, flow_id).await?;
        if !request.we_started() {
            return Err(OwnVerificationError::NotReady);
        }
        if self
            .own_sas(selection, cancellation, &client, &request)
            .await?
            .is_some()
        {
            return Ok(());
        }
        let info = snapshot(&request);
        if info.phase != OwnVerificationPhase::Ready {
            return Err(if info.is_open() {
                OwnVerificationError::NotReady
            } else {
                OwnVerificationError::Gone
            });
        }
        if info.offers_sas != Some(true) {
            return Err(OwnVerificationError::Unsupported);
        }
        let started = request.start_sas();
        let started = dispatch(selection, cancellation, admitted, SEND_TIMEOUT, started).await?;
        // `None`: the SDK found the request not ready after all and sent
        // nothing.
        started.map(|_| ()).ok_or(OwnVerificationError::NotReady)
    }

    /// Accept the SAS comparison the requesting device started. `false`
    /// while it has not started yet; nothing is sent then.
    pub async fn accept_own_sas_selected(
        &self,
        selection: &AccountSelection,
        cancellation: &VerificationCancellation,
        flow_id: &str,
        admitted: &(dyn Fn() -> bool + Send + Sync),
    ) -> OwnVerificationResult<bool> {
        let (client, request) = self.own_request(selection, cancellation, flow_id).await?;
        if request.we_started() {
            return Err(OwnVerificationError::NotReady);
        }
        let Some(sas) = self
            .own_sas(selection, cancellation, &client, &request)
            .await?
        else {
            return Ok(false);
        };
        if sas.is_cancelled() || sas.is_done() {
            return Err(OwnVerificationError::Gone);
        }
        dispatch(
            selection,
            cancellation,
            admitted,
            SEND_TIMEOUT,
            sas.accept(),
        )
        .await?;
        Ok(true)
    }

    /// The values to compare, once both devices can show them; `None`
    /// before. Never confirm based on these: the person compares them.
    pub async fn own_sas_values_selected(
        &self,
        selection: &AccountSelection,
        cancellation: &VerificationCancellation,
        flow_id: &str,
    ) -> OwnVerificationResult<Option<SasPresentation>> {
        let (client, request) = self.own_request(selection, cancellation, flow_id).await?;
        let Some(sas) = self
            .own_sas(selection, cancellation, &client, &request)
            .await?
        else {
            return Ok(None);
        };
        if sas.is_cancelled() || sas.is_done() {
            return Err(OwnVerificationError::Gone);
        }
        if !sas.can_be_presented() {
            return Ok(None);
        }
        Ok(Some(presentation(&sas)))
    }

    /// Confirm that the values match, after the person compared them on
    /// both devices and said so. Refused unless the values can be shown.
    pub async fn confirm_own_sas_selected(
        &self,
        selection: &AccountSelection,
        cancellation: &VerificationCancellation,
        flow_id: &str,
        admitted: &(dyn Fn() -> bool + Send + Sync),
    ) -> OwnVerificationResult<()> {
        let (client, request) = self.own_request(selection, cancellation, flow_id).await?;
        let sas = self
            .own_sas(selection, cancellation, &client, &request)
            .await?;
        let sas = sas.ok_or(OwnVerificationError::NotReady)?;
        if sas.is_cancelled() || sas.is_done() {
            return Err(OwnVerificationError::Gone);
        }
        if !sas.can_be_presented() {
            return Err(OwnVerificationError::NotReady);
        }
        dispatch(
            selection,
            cancellation,
            admitted,
            SEND_TIMEOUT,
            sas.confirm(),
        )
        .await
    }

    /// Cancel one flow (also "the values differ"). Cancelling grants no
    /// trust, so the security gate does not hold it back; the view and the
    /// selection still must be current.
    pub async fn cancel_own_verification_selected(
        &self,
        selection: &AccountSelection,
        cancellation: &VerificationCancellation,
        flow_id: &str,
    ) -> OwnVerificationResult<()> {
        let (_, request) = self.own_request(selection, cancellation, flow_id).await?;
        if !snapshot(&request).is_open() {
            return Ok(());
        }
        let always = || true;
        dispatch(
            selection,
            cancellation,
            &always,
            SEND_TIMEOUT,
            request.cancel(),
        )
        .await
    }
}

pub(crate) fn presentation(sas: &SasVerification) -> SasPresentation {
    let emoji = sas.emoji().map(|items| {
        items
            .into_iter()
            .map(|item| SasEmojiInfo {
                symbol: item.symbol.into(),
                description: item.description.into(),
            })
            .collect()
    });
    let decimals = sas.decimals().map(|(a, b, c)| [a, b, c]);
    SasPresentation { emoji, decimals }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names_are_bounded_and_plain() {
        assert_eq!(bounded_name(None), None);
        assert_eq!(bounded_name(Some("  \n ")), None);
        assert_eq!(
            bounded_name(Some("Desk\u{7}top")).as_deref(),
            Some("Desktop")
        );
        let long = "n".repeat(500);
        let bounded = bounded_name(Some(&long)).unwrap();
        assert_eq!(bounded.chars().count(), MAX_VERIFICATION_DEVICE_NAME_CHARS);
        assert!(offers_sas(&[VerificationMethod::SasV1]));
        assert!(!offers_sas(&[VerificationMethod::QrCodeScanV1]));
        assert!(!offers_sas(&[]));
    }

    fn recorded(flow: usize) -> crate::VerificationInfo {
        crate::VerificationInfo {
            flow_id: format!("flow-{flow}"),
            other_user_id: "@casey:example.org".into(),
            we_started: false,
            is_ready: false,
            is_done: false,
            is_cancelled: false,
            sent_at_ms: Some(1_000 + flow as u64),
        }
    }

    /// The producer keeps at most `MAX_RECORDED_VERIFICATIONS` (oldest out,
    /// no duplicates); a listing copies at most that many IDs, newest first,
    /// says when older ones may be missing, and counts the open requests it
    /// does not list.
    #[test]
    fn recorded_flows_and_listings_are_bounded_with_truthful_counts() {
        let mut list = Vec::new();
        for flow in 0..MAX_RECORDED_VERIFICATIONS * 3 {
            record_incoming(&mut list, recorded(flow));
            record_incoming(&mut list, recorded(flow));
            assert!(list.len() <= MAX_RECORDED_VERIFICATIONS);
        }
        let newest = MAX_RECORDED_VERIFICATIONS * 3 - 1;
        assert_eq!(list.last().unwrap().flow_id, format!("flow-{newest}"));
        let oldest = newest + 1 - MAX_RECORDED_VERIFICATIONS;
        assert_eq!(list.first().unwrap().flow_id, format!("flow-{oldest}"));
        let (candidates, full) = newest_recorded(&list);
        assert!(full);
        assert_eq!(candidates.len(), MAX_RECORDED_VERIFICATIONS);
        assert_eq!(candidates[0].0, format!("flow-{newest}"));
        assert_eq!(candidates[0].1, Some(1_000 + newest as u64));
        let (few, full) = newest_recorded(&list[..3]);
        assert_eq!((few.len(), full), (3, false));
        let open = |count: usize| {
            (0..count)
                .map(|flow| OwnVerification {
                    flow_id: format!("flow-{flow}"),
                    own_user_id: "@casey:example.org".into(),
                    device_id: None,
                    device_name: None,
                    we_started: false,
                    phase: OwnVerificationPhase::Requested,
                    offers_sas: Some(true),
                    sent_at_ms: None,
                })
                .collect::<Vec<_>>()
        };
        let listed = bounded_listing(open(MAX_PENDING_VERIFICATIONS + 5), true);
        assert_eq!(listed.requests.len(), MAX_PENDING_VERIFICATIONS);
        assert_eq!(listed.requests[0].flow_id, "flow-0");
        assert_eq!((listed.more_open, listed.older_may_be_omitted), (5, true));
        let listed = bounded_listing(open(2), false);
        assert_eq!((listed.requests.len(), listed.more_open), (2, 0));
    }

    /// One request per loaded account at a time, reserved before any async
    /// work and settled by the core: a second is refused while one is in
    /// flight; an unknown outcome blocks the next until acknowledged; an
    /// unsent one changes nothing; a reservation dropped after the hand-off
    /// counts as unknown, before it as unsent; sent flows stay bounded.
    #[test]
    fn requests_are_reserved_per_loaded_account_and_settled_truthfully() {
        let book = Arc::new(VerificationBook::default());
        let (mut first, flows) = Reservation::reserve(&book, false).unwrap();
        assert!(flows.is_empty());
        assert_eq!(
            Reservation::reserve(&book, true).err(),
            Some(OwnVerificationError::RequestInProgress)
        );
        first.hand_off();
        first.unknown();
        assert_eq!(
            Reservation::reserve(&book, false).err(),
            Some(OwnVerificationError::RequestUncertain)
        );
        // Acknowledged: allowed; unsent before the hand-off keeps the
        // uncertainty (nothing new was learned).
        let (mut second, _) = Reservation::reserve(&book, true).unwrap();
        second.unsent();
        assert!(book.with(|state| state.uncertain && !state.in_flight));
        // Sent: remembered, uncertainty resolved.
        let (mut third, _) = Reservation::reserve(&book, true).unwrap();
        third.hand_off();
        third.sent("flow-a");
        assert!(book.with(|state| !state.uncertain && state.outgoing == ["flow-a"]));
        // Dropped before the hand-off: unsent; after it: unknown.
        drop(Reservation::reserve(&book, false).unwrap());
        assert!(book.with(|state| !state.uncertain && !state.in_flight));
        let (mut dropped, _) = Reservation::reserve(&book, false).unwrap();
        dropped.hand_off();
        drop(dropped);
        assert!(book.with(|state| state.uncertain && !state.in_flight));
        // Bounded, no duplicates.
        for flow in 0..MAX_OUTGOING_FLOWS * 2 {
            book.with(|state| record_outgoing(state, &format!("flow-{flow}")));
        }
        let newest = format!("flow-{}", MAX_OUTGOING_FLOWS * 2 - 1);
        assert!(book.with(|state| state.outgoing.len() == MAX_OUTGOING_FLOWS));
        assert!(book.with(|state| state.outgoing.last() == Some(&newest)));
        book.with(|state| record_outgoing(state, &newest));
        assert!(book.with(|state| state.outgoing.len() == MAX_OUTGOING_FLOWS));
    }

    /// A late completion settles only into the loaded account's book it
    /// started on: a newer book (a reload or another sign-in) is never
    /// touched. Overflow of the incoming record is remembered after prunes.
    #[test]
    fn late_completions_and_overflow_stay_with_their_loaded_account() {
        let old = Arc::new(VerificationBook::default());
        let (mut late, _) = Reservation::reserve(&old, false).unwrap();
        let new = Arc::new(VerificationBook::default());
        late.hand_off();
        late.sent("flow-old");
        assert!(new.with(|state| state.outgoing.is_empty() && !state.in_flight));
        assert!(old.with(|state| state.outgoing == ["flow-old"]));
        let mut list = Vec::new();
        let mut dropped = false;
        for flow in 0..=MAX_RECORDED_VERIFICATIONS {
            dropped |= record_incoming(&mut list, recorded(flow));
        }
        assert!(dropped);
        new.mark_incoming_overflow();
        // A prune shrinks the record below the bound; the knowledge stays.
        list.truncate(3);
        let (_, full) = newest_recorded(&list);
        assert!(!full && (full || new.incoming_overflowed()));
        assert!(!old.incoming_overflowed());
    }

    #[test]
    fn only_open_flows_are_listed_as_open() {
        let mut info = OwnVerification {
            flow_id: "flow".into(),
            own_user_id: "@casey:example.org".into(),
            device_id: None,
            device_name: None,
            we_started: false,
            phase: OwnVerificationPhase::Requested,
            offers_sas: Some(true),
            sent_at_ms: None,
        };
        for (phase, open) in [
            (OwnVerificationPhase::Created, true),
            (OwnVerificationPhase::Requested, true),
            (OwnVerificationPhase::Ready, true),
            (OwnVerificationPhase::Comparing, true),
            (OwnVerificationPhase::Done, false),
            (OwnVerificationPhase::Cancelled { by_us: true }, false),
            (OwnVerificationPhase::Expired, false),
        ] {
            info.phase = phase;
            assert_eq!(info.is_open(), open, "{phase:?}");
        }
    }

    /// Nothing is handed to the SDK when the view is closed, the selection
    /// changed or the admission fence fails; once handed over, a stop is
    /// Unknown and the work is dropped, never retried.
    #[test]
    fn steps_are_fenced_before_hand_off_and_unknown_after() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let name = format!("morpheus-own-verification-{}", uuid::Uuid::new_v4());
            let root = std::env::temp_dir().join(name);
            let manager = AccountManager::open(&root).unwrap();
            let id = "account".to_owned();
            manager.selections.replace(Some(&id));
            let selection = manager.account_selection(&id).unwrap();
            let open = || true;
            let closed = || false;
            let sent = Arc::new(AtomicBool::new(false));
            let work = |sent: Arc<AtomicBool>| async move {
                sent.store(true, Ordering::SeqCst);
                Ok::<(), matrix_sdk::Error>(())
            };
            // The fence fails: nothing is handed over.
            let view = VerificationCancellation::new();
            let refused =
                dispatch(&selection, &view, &closed, SEND_TIMEOUT, work(sent.clone())).await;
            assert_eq!(refused, Err(OwnVerificationError::SecurityBusy));
            assert!(!sent.load(Ordering::SeqCst));
            // A closed view: nothing is handed over.
            let closed_view = VerificationCancellation::new();
            closed_view.cancel();
            let refused = dispatch(
                &selection,
                &closed_view,
                &open,
                SEND_TIMEOUT,
                work(sent.clone()),
            )
            .await;
            assert_eq!(refused, Err(OwnVerificationError::Cancelled));
            assert!(!sent.load(Ordering::SeqCst));
            // Admitted: handed over once.
            let accepted =
                dispatch(&selection, &view, &open, SEND_TIMEOUT, work(sent.clone())).await;
            assert_eq!(accepted, Ok(()));
            assert!(sent.load(Ordering::SeqCst));
            // Handed over and pending: a closed view or an account change
            // ends the wait at once and reports Unknown.
            for revoke in [false, true] {
                manager.selections.replace(Some(&id));
                let selection = manager.account_selection(&id).unwrap();
                let view = VerificationCancellation::new();
                let pending = std::future::pending::<matrix_sdk::Result<()>>();
                let step = dispatch(&selection, &view, &open, SEND_TIMEOUT, pending);
                let stop = async {
                    if revoke {
                        manager.selections.replace(None);
                    } else {
                        view.cancel();
                    }
                };
                let both = async { tokio::join!(step, stop) };
                let (result, ()) = time::timeout(Duration::from_secs(2), both).await.unwrap();
                assert_eq!(result, Err(OwnVerificationError::Unknown));
            }
            // A failed hand-off may still have left: Unknown, not "not sent".
            manager.selections.replace(Some(&id));
            let selection = manager.account_selection(&id).unwrap();
            let view = VerificationCancellation::new();
            let failed = async { Err::<(), _>(matrix_sdk::Error::InsufficientData) };
            let failed = dispatch(&selection, &view, &open, SEND_TIMEOUT, failed).await;
            assert_eq!(failed, Err(OwnVerificationError::Unknown));
            // A wait that sends nothing ends as Cancelled or AccountChanged.
            let pending = std::future::pending::<OwnVerificationResult<()>>();
            let view = VerificationCancellation::new();
            let wait = guarded(&selection, &view, LOCAL_TIMEOUT, pending);
            let stop = async { view.cancel() };
            let (waited, ()) = tokio::join!(wait, stop);
            assert_eq!(waited, Err(OwnVerificationError::Cancelled));
            manager.selections.replace(None);
            drop(manager);
            let _ = std::fs::remove_dir_all(&root);
        });
    }
}
