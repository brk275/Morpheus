//! Explicit peer SAS in one existing room. Own-device verification stays in
//! `own_verification`. No DM creation, invitation, manual trust or auto-confirm.
//! Each session owns its SDK request/SAS until its view ends; a public master-key
//! receipt from that exact SAS, not Done alone, identifies the verified person.

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use matrix_sdk::{
    Client, Room, RoomState,
    encryption::{
        identities::MasterPubkey,
        verification::{SasState, SasVerification, VerificationRequest},
    },
    ruma::{
        OwnedRoomId, OwnedUserId, RoomId, UserId,
        events::{
            key::verification::VerificationMethod,
            room::message::{MessageType, OriginalSyncRoomMessageEvent},
        },
    },
};

use crate::{
    AccountManager, AccountSelection, OwnVerification, OwnVerificationError as Error,
    OwnVerificationPhase, OwnVerificationResult as Result, SasPresentation,
    VERIFICATION_REQUEST_LIFETIME, VerificationCancellation,
    own_verification::{
        LOCAL_TIMEOUT, REQUEST_TIMEOUT, SEND_TIMEOUT, dispatch, guarded, presentation, snapshot,
    },
};

pub const MAX_PEER_REQUESTS: usize = 16;
const MAX_RECORDED: usize = 64;
const MAX_OUTGOING: usize = 8;
const MAX_FLOW_BYTES: usize = 1024;

/// Raw identifiers route operations; UI display escaping is never fed back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerVerificationTarget {
    pub user_id: OwnedUserId,
    pub room_id: OwnedRoomId,
}
impl PeerVerificationTarget {
    pub fn parse(user: &str, room: &str) -> Result<Self> {
        if user.len() > 255 || room.len() > 255 {
            return Err(Error::Unavailable);
        }
        Ok(Self {
            user_id: UserId::parse(user).map_err(|_| Error::Unavailable)?,
            room_id: RoomId::parse(room).map_err(|_| Error::Unavailable)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerVerificationInfo {
    pub target: PeerVerificationTarget,
    pub flow: OwnVerification,
    pub comparison_confirmed: bool,
}

#[derive(Clone)]
struct Recorded {
    target: PeerVerificationTarget,
    flow_id: String,
    received: Instant,
}
#[derive(Default)]
struct Book {
    incoming: Vec<Recorded>,
    outgoing: Vec<Recorded>,
    omitted: bool,
    in_flight: bool,
    uncertain: bool,
}
#[derive(Default)]
pub(crate) struct PeerVerificationBook(Mutex<Book>);
impl PeerVerificationBook {
    fn with<T>(&self, work: impl FnOnce(&mut Book) -> T) -> T {
        work(&mut self.0.lock().unwrap_or_else(|error| error.into_inner()))
    }
    fn record(&self, record: Recorded) {
        self.with(|book| {
            prune(&mut book.incoming);
            if book
                .incoming
                .iter()
                .any(|item| same_record(item, &record.target, &record.flow_id))
            {
                return;
            }
            book.incoming.push(record);
            let excess = book.incoming.len().saturating_sub(MAX_RECORDED);
            book.omitted |= excess > 0;
            book.incoming.drain(..excess);
        });
    }
}
fn prune(records: &mut Vec<Recorded>) {
    records.retain(|record| record.received.elapsed() < VERIFICATION_REQUEST_LIFETIME);
}
fn same_record(record: &Recorded, target: &PeerVerificationTarget, flow: &str) -> bool {
    record.target == *target && record.flow_id == flow
}

/// Registered once on this loaded client's existing SDK event dispatcher. The
/// callback does no IO, spawns no task and retains no client/loaded account.
/// Its bounded book belongs only to that loaded client, including late events.
pub(crate) fn register(client: &Client, book: Arc<PeerVerificationBook>) {
    let Some(own) = client.user_id().map(ToOwned::to_owned) else {
        return;
    };
    client.add_event_handler(move |event: OriginalSyncRoomMessageEvent, room: Room| {
        let own = own.clone();
        let book = book.clone();
        async move {
            let MessageType::VerificationRequest(content) = &event.content.msgtype else {
                return;
            };
            if event.sender == own || content.to != own || room.state() != RoomState::Joined {
                return;
            }
            let flow_id = event.event_id.to_string();
            if flow_id.len() > MAX_FLOW_BYTES {
                return;
            }
            book.record(Recorded {
                target: PeerVerificationTarget {
                    user_id: event.sender,
                    room_id: room.room_id().to_owned(),
                },
                flow_id,
                received: Instant::now(),
            });
        }
    });
}

#[derive(Clone, Debug, Default)]
pub struct PendingPeerVerifications {
    pub requests: Vec<PeerVerificationInfo>,
    pub more_open: usize,
    pub older_may_be_omitted: bool,
}

struct Reservation {
    book: Arc<PeerVerificationBook>,
    handed: bool,
    settled: bool,
}
impl Reservation {
    fn reserve(
        book: &Arc<PeerVerificationBook>,
        acknowledged: bool,
    ) -> Result<(Self, Vec<Recorded>)> {
        let records = book.with(|state| {
            if state.in_flight {
                return Err(Error::RequestInProgress);
            }
            if state.uncertain && !acknowledged {
                return Err(Error::RequestUncertain);
            }
            prune(&mut state.outgoing);
            state.in_flight = true;
            prune(&mut state.incoming);
            let mut records = state.outgoing.clone();
            records.extend(state.incoming.clone());
            records.sort_by_key(|record| record.received);
            Ok(records)
        })?;
        Ok((
            Self {
                book: book.clone(),
                handed: false,
                settled: false,
            },
            records,
        ))
    }
    fn settle(&mut self, record: Option<Recorded>, unknown: bool) {
        self.book.with(|book| {
            if let Some(record) = record {
                book.outgoing
                    .retain(|old| !same_record(old, &record.target, &record.flow_id));
                book.outgoing.push(record);
                let excess = book.outgoing.len().saturating_sub(MAX_OUTGOING);
                book.outgoing.drain(..excess);
                book.uncertain = false;
            }
            book.uncertain |= unknown;
            book.in_flight = false;
        });
        self.settled = true;
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.settled {
            self.book.with(|book| {
                book.in_flight = false;
                book.uncertain |= self.handed;
            });
        }
    }
}

/// State of this exact person's identity after an SDK-completed comparison.
/// A device-only result is not a verified cross-signing identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerVerificationOutcome {
    Pending,
    IdentityVerified,
    DeviceOnly,
    IdentityChanged,
    Unconfirmed,
}

struct Session {
    client: Client,
    selection: AccountSelection,
    cancellation: VerificationCancellation,
    own: OwnedUserId,
    target: PeerVerificationTarget,
    request: VerificationRequest,
    initial_master: Option<MasterPubkey>,
    opened: Instant,
    operation: tokio::sync::Mutex<()>,
    // Retain the actual bound SAS through cache GC and completion. No client
    // or SAS is stored in the book captured by the client's event handler.
    sas: tokio::sync::Mutex<Option<SasVerification>>,
}
#[derive(Clone)]
pub struct PeerVerificationSession(Arc<Session>);

fn context_matches(
    own: &UserId,
    target: &PeerVerificationTarget,
    request_own: &UserId,
    other: &UserId,
    room: Option<&RoomId>,
    self_verification: bool,
    passive: bool,
) -> bool {
    !self_verification
        && !passive
        && request_own == own
        && other == target.user_id
        && own != target.user_id
        && room == Some(target.room_id.as_ref())
}
fn binding(own: &UserId, target: &PeerVerificationTarget, request: &VerificationRequest) -> bool {
    context_matches(
        own,
        target,
        request.own_user_id(),
        request.other_user_id(),
        request.room_id(),
        request.is_self_verification(),
        request.is_passive(),
    )
}
fn sas_binding(own: &UserId, target: &PeerVerificationTarget, sas: &SasVerification) -> bool {
    !sas.is_self_verification()
        && sas.started_from_request()
        && sas.own_user_id() == own
        && sas.other_user_id() == target.user_id
        && sas.room_id() == Some(target.room_id.as_ref())
}

impl AccountManager {
    pub async fn pending_peer_verifications_selected(
        &self,
        selection: &AccountSelection,
        cancellation: &VerificationCancellation,
    ) -> Result<PendingPeerVerifications> {
        let scoped = self.verification_scope(selection, cancellation).await?;
        let (records, omitted) = scoped.loaded.peer_verifications.with(|book| {
            prune(&mut book.incoming);
            (book.incoming.clone(), book.omitted)
        });
        let mut requests = Vec::new();
        let mut retired = Vec::new();
        // One total bound, not MAX_RECORDED independent read timeouts.
        guarded(selection, cancellation, LOCAL_TIMEOUT, async {
            for record in records.iter().rev() {
                let request = scoped
                    .loaded
                    .client
                    .encryption()
                    .get_verification_request(&record.target.user_id, &record.flow_id)
                    .await;
                if let Some(request) = request
                    && binding(&scoped.own, &record.target, &request)
                    && !request.we_started()
                    && scoped
                        .loaded
                        .client
                        .get_room(&record.target.room_id)
                        .is_some_and(|room| room.state() == RoomState::Joined)
                {
                    let info = snapshot(&request);
                    if info.is_open() {
                        requests.push(PeerVerificationInfo {
                            target: record.target.clone(),
                            flow: info,
                            comparison_confirmed: false,
                        });
                        continue;
                    }
                }
                retired.push(record.clone());
            }
            Ok(())
        })
        .await?;
        scoped.loaded.peer_verifications.with(|book| {
            book.incoming.retain(|item| {
                !retired
                    .iter()
                    .any(|old| same_record(old, &item.target, &item.flow_id))
            });
        });
        let more_open = requests.len().saturating_sub(MAX_PEER_REQUESTS);
        requests.truncate(MAX_PEER_REQUESTS);
        Ok(PendingPeerVerifications {
            requests,
            more_open,
            older_may_be_omitted: omitted,
        })
    }

    pub async fn open_peer_verification_selected(
        &self,
        selection: &AccountSelection,
        cancellation: &VerificationCancellation,
        target: &PeerVerificationTarget,
        flow: &str,
    ) -> Result<PeerVerificationSession> {
        if flow.is_empty() || flow.len() > MAX_FLOW_BYTES {
            return Err(Error::Gone);
        }
        let scoped = self.verification_scope(selection, cancellation).await?;
        let opened = scoped.loaded.peer_verifications.with(|book| {
            prune(&mut book.incoming);
            prune(&mut book.outgoing);
            book.incoming
                .iter()
                .chain(book.outgoing.iter())
                .find(|record| same_record(record, target, flow))
                .map(|record| record.received)
                .ok_or(Error::Gone)
        })?;
        let request = guarded(selection, cancellation, LOCAL_TIMEOUT, async {
            scoped
                .loaded
                .client
                .encryption()
                .get_verification_request(&target.user_id, flow)
                .await
                .ok_or(Error::Gone)
        })
        .await?;
        if !binding(&scoped.own, target, &request) {
            return Err(Error::Gone);
        }
        let initial_master = guarded(selection, cancellation, REQUEST_TIMEOUT, async {
            scoped
                .loaded
                .client
                .encryption()
                .get_user_identity(&target.user_id)
                .await
                .map(|identity| identity.map(|value| value.master_key().clone()))
                .map_err(|_| Error::Unavailable)
        })
        .await?;
        let session = PeerVerificationSession(Arc::new(Session {
            client: scoped.loaded.client.clone(),
            selection: selection.clone(),
            cancellation: cancellation.clone(),
            own: scoped.own,
            target: target.clone(),
            request,
            initial_master,
            opened,
            operation: tokio::sync::Mutex::new(()),
            sas: tokio::sync::Mutex::new(None),
        }));
        session.info().await?;
        Ok(session)
    }

    pub async fn request_peer_verification_selected(
        &self,
        selection: &AccountSelection,
        cancellation: &VerificationCancellation,
        target: &PeerVerificationTarget,
        admitted: &(dyn Fn() -> bool + Send + Sync),
        acknowledged_uncertain: bool,
    ) -> Result<PeerVerificationSession> {
        let scoped = self.verification_scope(selection, cancellation).await?;
        if target.user_id == scoped.own {
            return Err(Error::Unavailable);
        }
        let (mut reservation, records) =
            Reservation::reserve(&scoped.loaded.peer_verifications, acknowledged_uncertain)?;
        for record in records
            .iter()
            .rev()
            .filter(|record| record.target.user_id == target.user_id)
        {
            match self
                .open_peer_verification_selected(
                    selection,
                    cancellation,
                    &record.target,
                    &record.flow_id,
                )
                .await
            {
                Ok(session) if session.info().await?.flow.is_open() => {
                    reservation.settle(None, false);
                    return Ok(session);
                }
                Err(Error::Gone) | Ok(_) => {}
                Err(error) => return Err(error),
            }
        }
        let (prepared, initial_master) = guarded(selection, cancellation, REQUEST_TIMEOUT, async {
            let identity = scoped
                .loaded
                .client
                .encryption()
                .get_user_identity(&target.user_id)
                .await
                .map_err(|_| Error::Unavailable)?
                .ok_or(Error::Unavailable)?;
            let master = identity.master_key().clone();
            let prepared = identity
                .prepare_verification_in_room_with_methods(
                    &target.room_id,
                    vec![VerificationMethod::SasV1],
                )
                .await
                .map_err(|_| Error::Unavailable)?;
            Ok((prepared, Some(master)))
        })
        .await?;
        reservation.handed = true;
        let request = match dispatch(
            selection,
            cancellation,
            admitted,
            SEND_TIMEOUT,
            prepared.send(),
        )
        .await
        {
            Ok(request) => request,
            Err(error) => {
                reservation.settle(None, error == Error::Unknown);
                return Err(error);
            }
        };
        if !binding(&scoped.own, target, &request) {
            reservation.settle(None, true);
            return Err(Error::Unknown);
        }
        reservation.settle(
            Some(Recorded {
                target: target.clone(),
                flow_id: request.flow_id().to_owned(),
                received: Instant::now(),
            }),
            false,
        );
        Ok(PeerVerificationSession(Arc::new(Session {
            client: scoped.loaded.client.clone(),
            selection: selection.clone(),
            cancellation: cancellation.clone(),
            own: scoped.own,
            target: target.clone(),
            request,
            initial_master,
            opened: Instant::now(),
            operation: tokio::sync::Mutex::new(()),
            sas: tokio::sync::Mutex::new(None),
        })))
    }
}

impl PeerVerificationSession {
    pub fn target(&self) -> &PeerVerificationTarget {
        &self.0.target
    }
    pub fn flow_id(&self) -> &str {
        self.0.request.flow_id()
    }
    fn check(&self) -> Result<()> {
        crate::own_verification::scope_error(&self.0.selection, &self.0.cancellation)?;
        if self.0.opened.elapsed() >= VERIFICATION_REQUEST_LIFETIME {
            return Err(Error::Gone);
        }
        if self.0.client.user_id() != Some(self.0.own.as_ref())
            || !binding(&self.0.own, &self.0.target, &self.0.request)
            || !self
                .0
                .client
                .get_room(&self.0.target.room_id)
                .is_some_and(|room| room.state() == RoomState::Joined)
        {
            return Err(Error::Gone);
        }
        Ok(())
    }
    async fn operation(&self) -> Result<tokio::sync::MutexGuard<'_, ()>> {
        self.check()?;
        let guard = guarded(
            &self.0.selection,
            &self.0.cancellation,
            LOCAL_TIMEOUT,
            async { Ok(self.0.operation.lock().await) },
        )
        .await?;
        self.check()?;
        Ok(guard)
    }
    async fn sas(&self) -> Result<Option<SasVerification>> {
        self.check()?;
        guarded(
            &self.0.selection,
            &self.0.cancellation,
            LOCAL_TIMEOUT,
            async {
                let mut held = self.0.sas.lock().await;
                if held.is_none() {
                    let found = self
                        .0
                        .client
                        .encryption()
                        .get_verification(&self.0.target.user_id, self.flow_id())
                        .await;
                    if let Some(sas) = found.and_then(|value| value.sas()) {
                        if !sas_binding(&self.0.own, &self.0.target, &sas) {
                            return Err(Error::Gone);
                        }
                        *held = Some(sas);
                    }
                }
                Ok(held.clone())
            },
        )
        .await
    }
    pub async fn info(&self) -> Result<PeerVerificationInfo> {
        self.check()?;
        // Capture an already-started SAS before SDK request/cache GC. The
        // retained handle is the only source of the completion identity receipt.
        let sas = self.sas().await?;
        self.check()?;
        let comparison_confirmed = sas
            .is_some_and(|sas| matches!(sas.state(), SasState::Confirmed | SasState::Done { .. }));
        Ok(PeerVerificationInfo {
            target: self.0.target.clone(),
            flow: snapshot(&self.0.request),
            comparison_confirmed,
        })
    }
    pub async fn accept(&self, admitted: &(dyn Fn() -> bool + Send + Sync)) -> Result<()> {
        let _operation = self.operation().await?;
        let info = self.info().await?.flow;
        if info.we_started || info.phase != OwnVerificationPhase::Requested {
            return Err(Error::NotReady);
        }
        if info.offers_sas != Some(true) {
            return Err(Error::Unsupported);
        }
        dispatch(
            &self.0.selection,
            &self.0.cancellation,
            admitted,
            SEND_TIMEOUT,
            self.0
                .request
                .accept_with_methods(vec![VerificationMethod::SasV1]),
        )
        .await
    }
    pub async fn advance(&self, admitted: &(dyn Fn() -> bool + Send + Sync)) -> Result<bool> {
        let _operation = self.operation().await?;
        let info = self.info().await?.flow;
        if !info.is_open() {
            return Err(Error::Gone);
        }
        if info.offers_sas == Some(false) {
            return Err(Error::Unsupported);
        }
        if let Some(sas) = self.sas().await? {
            if !info.we_started && matches!(sas.state(), SasState::Started { .. }) {
                dispatch(
                    &self.0.selection,
                    &self.0.cancellation,
                    admitted,
                    SEND_TIMEOUT,
                    sas.accept(),
                )
                .await?;
            }
            return Ok(true);
        }
        if !info.we_started {
            return Ok(false);
        }
        if info.phase != OwnVerificationPhase::Ready {
            return Err(Error::NotReady);
        }
        let started = dispatch(
            &self.0.selection,
            &self.0.cancellation,
            admitted,
            SEND_TIMEOUT,
            self.0.request.start_sas(),
        )
        .await?;
        let Some(sas) = started else { return Ok(false) };
        if !sas_binding(&self.0.own, &self.0.target, &sas) {
            return Err(Error::Unknown);
        }
        guarded(
            &self.0.selection,
            &self.0.cancellation,
            LOCAL_TIMEOUT,
            async {
                *self.0.sas.lock().await = Some(sas);
                Ok(())
            },
        )
        .await
        .map_err(|_| Error::Unknown)?;
        Ok(true)
    }
    pub async fn values(&self) -> Result<Option<SasPresentation>> {
        let Some(sas) = self.sas().await? else {
            return Ok(None);
        };
        if sas.is_cancelled() || sas.is_done() {
            return Err(Error::Gone);
        }
        Ok(matches!(sas.state(), SasState::KeysExchanged { .. }).then(|| presentation(&sas)))
    }
    pub async fn confirm(&self, admitted: &(dyn Fn() -> bool + Send + Sync)) -> Result<()> {
        let _operation = self.operation().await?;
        let sas = self.sas().await?.ok_or(Error::NotReady)?;
        if sas.is_cancelled() || sas.is_done() {
            return Err(Error::Gone);
        }
        if !sas.can_be_presented() || !matches!(sas.state(), SasState::KeysExchanged { .. }) {
            return Err(Error::NotReady);
        }
        dispatch(
            &self.0.selection,
            &self.0.cancellation,
            admitted,
            SEND_TIMEOUT,
            sas.confirm(),
        )
        .await
    }
    pub async fn cancel(&self, admitted: &(dyn Fn() -> bool + Send + Sync)) -> Result<()> {
        let _operation = self.operation().await?;
        self.check()?;
        dispatch(
            &self.0.selection,
            &self.0.cancellation,
            admitted,
            SEND_TIMEOUT,
            self.0.request.cancel(),
        )
        .await
    }
    pub async fn outcome(&self) -> Result<PeerVerificationOutcome> {
        let Some(sas) = self.sas().await? else {
            return Ok(PeerVerificationOutcome::Unconfirmed);
        };
        let SasState::Done {
            verified_identities,
            ..
        } = sas.state()
        else {
            return Ok(PeerVerificationOutcome::Pending);
        };
        let verified = verified_identities
            .iter()
            .find(|identity| identity.user_id() == self.0.target.user_id);
        let expected = verified.map(|identity| identity.master_key().clone());
        let actual = guarded(
            &self.0.selection,
            &self.0.cancellation,
            REQUEST_TIMEOUT,
            async {
                self.0
                    .client
                    .encryption()
                    .get_user_identity(&self.0.target.user_id)
                    .await
                    .map_err(|_| Error::Unavailable)
            },
        )
        .await?;
        self.check()?;
        let Some(actual) = actual else {
            return Ok(if expected.is_some() || self.0.initial_master.is_some() {
                PeerVerificationOutcome::IdentityChanged
            } else {
                PeerVerificationOutcome::Unconfirmed
            });
        };
        if expected.is_none()
            && self
                .0
                .initial_master
                .as_ref()
                .is_some_and(|initial| initial != actual.master_key())
        {
            return Ok(PeerVerificationOutcome::IdentityChanged);
        }
        Ok(identity_outcome(
            expected.as_ref(),
            actual.master_key(),
            actual.is_verified(),
        ))
    }
}
fn identity_outcome(
    receipt: Option<&MasterPubkey>,
    current: &MasterPubkey,
    verified: bool,
) -> PeerVerificationOutcome {
    match receipt {
        None => PeerVerificationOutcome::DeviceOnly,
        Some(expected) if expected != current => PeerVerificationOutcome::IdentityChanged,
        Some(_) if verified => PeerVerificationOutcome::IdentityVerified,
        Some(_) => PeerVerificationOutcome::DeviceOnly,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{room_id, user_id};
    use serde_json::json;

    #[test]
    fn peer_context_rejects_foreign_accounts_people_rooms_passive_and_self_flows() {
        let own = user_id!("@alice:example.org");
        let peer = user_id!("@bob:example.org");
        let foreign = user_id!("@charlie:example.org");
        let room = room_id!("!dm:example.org");
        let other_room = room_id!("!elsewhere:example.org");
        let target = PeerVerificationTarget::parse(peer.as_str(), room.as_str()).unwrap();
        assert!(context_matches(
            own,
            &target,
            own,
            peer,
            Some(room),
            false,
            false
        ));
        assert!(!context_matches(
            own,
            &target,
            foreign,
            peer,
            Some(room),
            false,
            false
        ));
        assert!(!context_matches(
            own,
            &target,
            own,
            foreign,
            Some(room),
            false,
            false
        ));
        assert!(!context_matches(
            own,
            &target,
            own,
            peer,
            Some(other_room),
            false,
            false
        ));
        assert!(!context_matches(
            own, &target, own, peer, None, false, false
        ));
        assert!(!context_matches(
            own,
            &target,
            own,
            peer,
            Some(room),
            true,
            false
        ));
        assert!(!context_matches(
            own,
            &target,
            own,
            peer,
            Some(room),
            false,
            true
        ));
        let self_target = PeerVerificationTarget::parse(own.as_str(), room.as_str()).unwrap();
        assert!(!context_matches(
            own,
            &self_target,
            own,
            own,
            Some(room),
            false,
            false
        ));
        assert!(PeerVerificationTarget::parse("not-a-user", room.as_str()).is_err());
        assert!(PeerVerificationTarget::parse(peer.as_str(), "not-a-room").is_err());
    }

    #[test]
    fn incoming_and_outgoing_books_keep_exact_context_and_bounded_lifetimes() {
        let target = |peer, room| PeerVerificationTarget::parse(peer, room).unwrap();
        let first = target("@bob:example.org", "!a:example.org");
        let second = target("@charlie:example.org", "!a:example.org");
        let third = target("@bob:example.org", "!b:example.org");
        let record = |target| Recorded {
            target,
            flow_id: "$same-event-id".into(),
            received: Instant::now(),
        };
        let book = Arc::new(PeerVerificationBook::default());
        book.record(record(first.clone()));
        book.record(record(first.clone()));
        book.record(record(second));
        book.record(record(third));
        assert_eq!(book.with(|book| book.incoming.len()), 3);
        // All rooms for one peer participate in outgoing duplicate protection.
        let (mut reserved, records) = Reservation::reserve(&book, false).unwrap();
        assert_eq!(
            records
                .iter()
                .filter(|record| record.target.user_id == first.user_id)
                .count(),
            2
        );
        assert_eq!(
            Reservation::reserve(&book, true).err().map(|_| ()),
            Some(())
        );
        reserved.handed = true;
        drop(reserved);
        assert_eq!(
            Reservation::reserve(&book, false).err().map(|_| ()),
            Some(())
        );
        let (mut acknowledged, _) = Reservation::reserve(&book, true).unwrap();
        acknowledged.settle(Some(record(first.clone())), false);
        assert!(!book.with(|state| state.uncertain));
        for n in 0..MAX_RECORDED + 2 {
            book.record(Recorded {
                target: first.clone(),
                flow_id: format!("$flow-{n}"),
                received: Instant::now(),
            });
        }
        assert!(book.with(|state| state.omitted && state.incoming.len() == MAX_RECORDED));
        book.with(|state| {
            state.incoming[0].received = Instant::now()
                .checked_sub(VERIFICATION_REQUEST_LIFETIME)
                .unwrap();
            prune(&mut state.incoming);
            assert_eq!(state.incoming.len(), MAX_RECORDED - 1);
            assert!(state.omitted);
        });
    }

    #[test]
    fn completed_device_comparison_does_not_substitute_for_exact_verified_identity() {
        fn master(key: &str) -> MasterPubkey {
            serde_json::from_value(json!({
                "user_id": "@bob:example.org", "usage": ["master"],
                "keys": {(format!("ed25519:{key}")): key}
            }))
            .unwrap()
        }
        // Public fixture keys only; no private signing material.
        let first = master("3NZwYz0VjFrhONhYT2iBWCYdEYF266jn/vmZqc6QdDU");
        let changed = master("5JpU6BNHsBZbf4Y3t0IvpIWa7kKDSGy3b+DjVjUuJmc");
        assert_eq!(
            identity_outcome(None, &first, true),
            PeerVerificationOutcome::DeviceOnly
        );
        assert_eq!(
            identity_outcome(Some(&first), &first, false),
            PeerVerificationOutcome::DeviceOnly
        );
        assert_eq!(
            identity_outcome(Some(&first), &first, true),
            PeerVerificationOutcome::IdentityVerified
        );
        assert_eq!(
            identity_outcome(Some(&first), &changed, true),
            PeerVerificationOutcome::IdentityChanged
        );
    }
}
