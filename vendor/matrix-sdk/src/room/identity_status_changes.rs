// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Facility to track changes to the identity of members of rooms.
#![cfg(feature = "e2e-encryption")]

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_stream::stream;
use futures_core::Stream;
use futures_util::{StreamExt, stream_select};
use matrix_sdk_base::{
    RoomMemberships,
    crypto::{
        IdentityState, IdentityStatusChange, RoomIdentityChange, RoomIdentityProvider,
        RoomIdentityState,
    },
};
use matrix_sdk_common::BoxFuture;
use ruma::{
    OwnedUserId, UserId,
    events::room::member::{MembershipState, SyncRoomMemberEvent},
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::Room;
use crate::{
    Client, Error, Result,
    encryption::identities::{IdentityUpdates, UserIdentity},
    event_handler::EventHandlerDropGuard,
};

/// Support for creating a stream of batches of [`IdentityStatusChange`].
///
/// Internally, this subscribes to all identity changes, and to room events that
/// change the membership, and provides a stream of all changes to the identity
/// status of all room members.
///
/// This struct does not represent the actual stream, but the state that is used
/// to produce the values of the stream.
///
/// It does provide a method to create the stream:
/// [`IdentityStatusChanges::create_stream`].
#[derive(Debug)]
pub struct IdentityStatusChanges {
    /// Who is in the room and who is in identity violation at this moment
    room_identity_state: RoomIdentityState<MemberSource>,

    /// Dropped when we are dropped, and unregisters the event handler we
    /// registered to listen for room events
    _drop_guard: EventHandlerDropGuard,
}

impl IdentityStatusChanges {
    /// Create a new stream of significant changes to the identity status of
    /// members of a room.
    ///
    /// The "status" of an identity changes when our level of trust in it
    /// changes.
    ///
    /// A "significant" change means a warning should either be added or removed
    /// (e.g. the user changed from pinned to unpinned (show a warning) or from
    /// verification violation to pinned (remove a warning). An insignificant
    /// change would be from pinned to verified - no warning is needed in this
    /// case.
    ///
    /// For example, if an identity is "pinned" i.e. not manually verified, but
    /// known, and it becomes a "unpinned" i.e. unknown, because the encryption
    /// keys are different and the user has not acknowledged this, then this
    /// constitutes a status change. Also, if an identity is "unpinned" and
    /// becomes "pinned", this is also a status change.
    ///
    /// The supplied stream is intended to provide enough information for a
    /// client to display a list of room members whose identities have changed,
    /// and allow the user to acknowledge this or act upon it.
    ///
    /// The first item in the stream provides the current state of the room:
    /// each member of the room who is not in "pinned" or "verified" state will
    /// be included (except the current user).
    ///
    /// Note: when an unpinned user leaves a room, an update is generated
    /// stating that they have become pinned, even though they may not
    /// necessarily have become pinned, but we don't care any more because they
    /// left the room.
    pub async fn create_stream(
        room: Room,
    ) -> Result<impl Stream<Item = Vec<IdentityStatusChange>>> {
        Self::create_stream_from(MemberSource::Room(room)).await
    }

    /// Like [`IdentityStatusChanges::create_stream`], but membership is read
    /// only from the room members already stored on this device: the member
    /// list is never requested from the homeserver, and a stored member counts
    /// only while joined or invited. Identities come from the local crypto
    /// store, as with [`IdentityStatusChanges::create_stream`].
    ///
    /// A local read that fails is never taken for an empty answer: if the
    /// stored members or their identities cannot be read for the initial
    /// state, this returns that error and no stream; if a later read fails,
    /// the stream ends.
    /// Queued changes are reconciled against current stored membership;
    /// departed retained identities are forgotten before publication.
    pub(crate) async fn create_stream_no_sync(
        room: Room,
    ) -> Result<impl Stream<Item = Vec<IdentityStatusChange>>> {
        Self::create_stream_from(MemberSource::Stored(room, StoredReads::default())).await
    }

    /// The stream lifecycle shared by both entry points. The stored source
    /// additionally reconciles queued changes with current local membership.
    async fn create_stream_from(
        members: MemberSource,
    ) -> Result<impl Stream<Item = Vec<IdentityStatusChange>>> {
        let room = members.room();
        let identity_updates = wrap_identity_updates(&room.client).await?;
        let (drop_guard, room_member_events) = wrap_room_member_events(room);
        let mut unprocessed_stream = combine_streams(identity_updates, room_member_events);
        let own_user_id = room.client.user_id().ok_or(Error::InsufficientData)?.to_owned();
        let stored_reads = members.stored_reads();
        let stored_members = match &members {
            MemberSource::Stored(_, _) => Some(members.clone()),
            MemberSource::Room(_) => None,
        };

        let state = IdentityStatusChanges {
            room_identity_state: RoomIdentityState::new(members).await,
            _drop_guard: drop_guard,
        };

        // The stored-members source starts only from a state it actually
        // read: an unreadable member list is not an empty one.
        if let Some(error) = stored_reads.as_ref().and_then(StoredReads::take) {
            return Err(error);
        }

        Ok(stream!({
            // Force enclosing stream to take ownership of state, so that
            // _drop_guard does not get dropped.
            let mut state = state;

            let mut current_state =
                filter_for_initial_update(state.room_identity_state.current_state(), &own_user_id);

            if !current_state.is_empty() {
                current_state.sort();
                yield current_state;
            }

            while let Some(item) = unprocessed_stream.next().await {
                let changes = if let Some(members) = &stored_members {
                    state.process_stored_change(item, members).await
                } else {
                    state.room_identity_state.process_change(item).await
                };
                let mut update = filter_non_self(changes, &own_user_id);
                // A stored read that failed while processing this change
                // leaves the state unknown: end the stream rather than
                // report it as if nothing were missing.
                if stored_reads.as_ref().is_some_and(StoredReads::failed) {
                    break;
                }
                if !update.is_empty() {
                    update.sort();
                    yield update;
                }
            }
        }))
    }

    /// Reconcile queued changes against current stored membership. In the
    /// upstream state machine with active-only admission, a queued pin or
    /// verification is ignored after a departure, and the departure itself
    /// is ignored once the identity is pinned/verified. Rebuild only when an
    /// affected retained identity has actually departed, so its internal
    /// state is forgotten as well as its warning. A later active rejoin can
    /// then report a violation again.
    async fn process_stored_change(
        &mut self,
        item: RoomIdentityChange,
        members: &MemberSource,
    ) -> Vec<IdentityStatusChange> {
        let before: BTreeMap<_, _> = self
            .room_identity_state
            .current_state()
            .into_iter()
            .map(|change| (change.user_id, change.changed_to))
            .collect();
        let mut departed = false;
        let mut active_membership_event = false;
        match &item {
            RoomIdentityChange::IdentityUpdates(updates) => {
                for user_id in updates.new.keys().chain(updates.changed.keys()) {
                    if before.contains_key(user_id) && !members.is_member(user_id).await {
                        departed = true;
                    }
                }
            }
            RoomIdentityChange::SyncRoomMemberEvent(event) => {
                if event.as_original().is_none() {
                    return Vec::new();
                }
                let active = members.is_member(event.state_key()).await;
                departed = before.contains_key(event.state_key()) && !active;
                active_membership_event = active
                    && matches!(
                        event.membership(),
                        MembershipState::Join | MembershipState::Invite
                    );
            }
        }
        if members.stored_reads().is_some_and(|reads| reads.failed()) {
            return Vec::new();
        }

        if departed {
            // O(room-size) local reads, once for this detected departure batch.
            // Dropping the stream cancels this await; no task is spawned.
            let rebuilt = RoomIdentityState::new(members.clone()).await;
            if members.stored_reads().is_some_and(|reads| reads.failed()) {
                return Vec::new();
            }
            let after: BTreeMap<_, _> = rebuilt
                .current_state()
                .into_iter()
                .map(|change| (change.user_id, change.changed_to))
                .collect();
            let mut changes = Vec::new();
            for user_id in before.keys().filter(|user_id| !after.contains_key(*user_id)) {
                // The existing stream API uses Pinned as the removal signal
                // for departed members. This does not assert or mutate their
                // actual crypto identity or trust.
                changes.push(IdentityStatusChange {
                    user_id: user_id.clone(),
                    changed_to: IdentityState::Pinned,
                });
            }
            for (user_id, changed_to) in after {
                if before.get(&user_id) != Some(&changed_to) {
                    changes.push(IdentityStatusChange { user_id, changed_to });
                }
            }
            self.room_identity_state = rebuilt;
            return changes;
        }

        // Membership events may be older than the stored state. A queued
        // Join must not resurrect a departed warning, nor a queued Leave
        // remove a current active rejoin. Identity updates retain the stored
        // provider's Join/Invite admission check.
        if matches!(&item, RoomIdentityChange::SyncRoomMemberEvent(_)) && !active_membership_event {
            return Vec::new();
        }
        self.room_identity_state.process_change(item).await
    }
}

/// Where an [`IdentityStatusChanges`] stream reads the room's membership.
#[derive(Clone, Debug)]
enum MemberSource {
    /// [`Room`]'s own [`RoomIdentityProvider`], unchanged: it requests the
    /// member list from the homeserver when this device's copy is incomplete.
    Room(Room),
    /// Only the members stored on this device. Never requests the member
    /// list; a stored member who left or was banned is not a member. A local
    /// read that fails is recorded in [`StoredReads`], never taken for an
    /// empty answer.
    Stored(Room, StoredReads),
}

impl MemberSource {
    fn room(&self) -> &Room {
        match self {
            Self::Room(room) | Self::Stored(room, _) => room,
        }
    }

    /// The failure record of the stored-members source; `None` for
    /// [`MemberSource::Room`], whose behaviour is unchanged.
    fn stored_reads(&self) -> Option<StoredReads> {
        match self {
            Self::Room(_) => None,
            Self::Stored(_, reads) => Some(reads.clone()),
        }
    }
}

/// The first local read of a [`MemberSource::Stored`] stream that failed.
///
/// [`RoomIdentityProvider`] has no error path: its results are a plain list,
/// `bool` or `Option`. The stored source records a failed read here instead
/// of answering as if the member or identity were absent, so the stream can
/// refuse to start (initial state) or end (later change).
#[derive(Clone, Debug, Default)]
struct StoredReads(Arc<Mutex<Option<Error>>>);

impl StoredReads {
    fn record(&self, error: impl Into<Error>) {
        let mut first = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if first.is_none() {
            *first = Some(error.into());
        }
    }

    fn failed(&self) -> bool {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).is_some()
    }

    fn take(&self) -> Option<Error> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take()
    }
}

/// One identity from the local crypto store (`OlmMachine::get_identity` with
/// no key-query wait), recording a failed read in `reads`.
async fn stored_identity(
    room: &Room,
    reads: &StoredReads,
    user_id: &UserId,
) -> Option<matrix_sdk_base::crypto::UserIdentity> {
    match room.client.encryption().get_user_identity(user_id).await {
        Ok(identity) => identity.map(|identity| identity.underlying_identity()),
        Err(error) => {
            reads.record(error);
            None
        }
    }
}

impl RoomIdentityProvider for MemberSource {
    fn is_member<'a>(&'a self, user_id: &'a UserId) -> BoxFuture<'a, bool> {
        match self {
            Self::Room(room) => RoomIdentityProvider::is_member(room, user_id),
            Self::Stored(room, reads) => Box::pin(async move {
                match room.get_member_no_sync(user_id).await {
                    Ok(Some(member)) => matches!(
                        member.membership(),
                        MembershipState::Join | MembershipState::Invite
                    ),
                    Ok(None) => false,
                    Err(error) => {
                        reads.record(error);
                        false
                    }
                }
            }),
        }
    }

    fn member_identities(&self) -> BoxFuture<'_, Vec<matrix_sdk_base::crypto::UserIdentity>> {
        match self {
            Self::Room(room) => RoomIdentityProvider::member_identities(room),
            Self::Stored(room, reads) => Box::pin(async move {
                let members = match room
                    .members_no_sync(RoomMemberships::JOIN | RoomMemberships::INVITE)
                    .await
                {
                    Ok(members) => members,
                    Err(error) => {
                        reads.record(error);
                        return Vec::new();
                    }
                };

                let mut ret = Vec::new();
                for member in members {
                    if let Some(identity) = stored_identity(room, reads, member.user_id()).await {
                        ret.push(identity);
                    }
                }
                ret
            }),
        }
    }

    /// The local crypto store only, for both sources; the stored source
    /// records a failed read.
    fn user_identity<'a>(
        &'a self,
        user_id: &'a UserId,
    ) -> BoxFuture<'a, Option<matrix_sdk_base::crypto::UserIdentity>> {
        match self {
            Self::Room(room) => RoomIdentityProvider::user_identity(room, user_id),
            Self::Stored(room, reads) => Box::pin(stored_identity(room, reads, user_id)),
        }
    }
}

fn filter_for_initial_update(
    mut input: Vec<IdentityStatusChange>,
    own_user_id: &UserId,
) -> Vec<IdentityStatusChange> {
    // We are never interested in changes to our own identity, and also for
    // initial updates, we are only interested in "bad" states where we need to
    // notify the user, so we can remove Verified states (Pinned states are
    // already missing, because Pinned is considered the default).
    input.retain(|change| {
        change.user_id != own_user_id && change.changed_to != IdentityState::Verified
    });

    input
}

fn filter_non_self(
    mut input: Vec<IdentityStatusChange>,
    own_user_id: &UserId,
) -> Vec<IdentityStatusChange> {
    // We are never interested in changes to our own identity
    input.retain(|change| change.user_id != own_user_id);
    input
}

fn combine_streams(
    identity_updates: impl Stream<Item = RoomIdentityChange> + Unpin,
    room_member_events: impl Stream<Item = RoomIdentityChange> + Unpin,
) -> impl Stream<Item = RoomIdentityChange> {
    stream_select!(identity_updates, room_member_events)
}

async fn wrap_identity_updates(
    client: &Client,
) -> Result<impl Stream<Item = RoomIdentityChange> + use<>> {
    Ok(client
        .encryption()
        .user_identities_stream()
        .await?
        .map(|item| RoomIdentityChange::IdentityUpdates(to_base_updates(item))))
}

fn to_base_updates(
    input: IdentityUpdates,
) -> matrix_sdk_base::crypto::store::types::IdentityUpdates {
    matrix_sdk_base::crypto::store::types::IdentityUpdates {
        new: to_base_identities(input.new),
        changed: to_base_identities(input.changed),
        unchanged: Default::default(),
    }
}

fn to_base_identities(
    input: BTreeMap<OwnedUserId, UserIdentity>,
) -> BTreeMap<OwnedUserId, matrix_sdk_base::crypto::UserIdentity> {
    input.into_iter().map(|(k, v)| (k, v.underlying_identity())).collect()
}

fn wrap_room_member_events(
    room: &Room,
) -> (EventHandlerDropGuard, impl Stream<Item = RoomIdentityChange> + use<>) {
    let own_user_id = room.own_user_id().to_owned();
    let room_id = room.room_id();
    let (sender, receiver) = mpsc::channel(16);
    let handle =
        room.client.add_room_event_handler(room_id, move |event: SyncRoomMemberEvent| async move {
            if *event.state_key() == own_user_id {
                return;
            }
            let _: Result<_, _> =
                sender.send(RoomIdentityChange::SyncRoomMemberEvent(Box::new(event))).await;
        });
    let drop_guard = room.client.event_handler_drop_guard(handle);
    (drop_guard, ReceiverStream::new(receiver))
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::time::Duration;

    use futures_util::{FutureExt as _, StreamExt as _, pin_mut};
    use matrix_sdk_base::{RoomMemberships, crypto::IdentityState};
    use matrix_sdk_test::{async_test, test_json::keys_query_sets::IdentityChangeDataSet};
    use ruma::events::room::member::MembershipState;
    use test_setup::TestSetup;

    use crate::assert_next_with_timeout;

    #[async_test]
    async fn test_when_user_becomes_unpinned_we_report_it() {
        // Given a room containing us and Bob
        let t = TestSetup::new_room_with_other_bob().await;

        // And Bob's identity is pinned
        t.pin_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob becomes unpinned
        t.unpin_bob().await;

        // Then we were notified about it
        let change = assert_next_with_timeout!(stream);
        assert_eq!(change[0].user_id, t.bob_user_id());
        assert_eq!(change[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change.len(), 1);
    }

    #[async_test]
    async fn test_when_user_becomes_verification_violation_we_report_it() {
        // Given a room containing us and Bob
        let t = TestSetup::new_room_with_other_bob().await;

        // And Bob's identity is verified
        t.verify_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob's identity changes
        t.unpin_bob().await;

        // Then we were notified about a verification violation
        let change = assert_next_with_timeout!(stream);
        assert_eq!(change[0].user_id, t.bob_user_id());
        assert_eq!(change[0].changed_to, IdentityState::VerificationViolation);
        assert_eq!(change.len(), 1);
    }

    #[async_test]
    async fn test_when_user_becomes_pinned_we_report_it() {
        // Given a room containing us and Bob
        let t = TestSetup::new_room_with_other_bob().await;

        // And Bob's identity is unpinned
        t.unpin_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob becomes pinned
        t.pin_bob().await;

        // Then we were notified about the initial state of the room
        let change1 = assert_next_with_timeout!(stream);
        assert_eq!(change1[0].user_id, t.bob_user_id());
        assert_eq!(change1[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change1.len(), 1);

        // And the change when Bob became pinned
        let change2 = assert_next_with_timeout!(stream);
        assert_eq!(change2[0].user_id, t.bob_user_id());
        assert_eq!(change2[0].changed_to, IdentityState::Pinned);
        assert_eq!(change2.len(), 1);
    }

    #[async_test]
    async fn test_when_user_becomes_verified_we_report_it() {
        // Given a room containing us and Bob
        let t = TestSetup::new_room_with_other_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob becomes verified
        t.verify_bob().await;

        // Then we are notified about Bob being verified
        let change = assert_next_with_timeout!(stream);
        assert_eq!(change[0].user_id, t.bob_user_id());
        assert_eq!(change[0].changed_to, IdentityState::Verified);
        assert_eq!(change.len(), 1);

        // (And then unpinned, so we have something to come through the stream)
        t.unpin_bob().await;

        // Then we are notified about the unpinning part
        let change = assert_next_with_timeout!(stream);
        assert_eq!(change[0].user_id, t.bob_user_id());
        assert_eq!(change[0].changed_to, IdentityState::VerificationViolation);
        assert_eq!(change.len(), 1);
    }

    #[async_test]
    async fn test_when_an_unpinned_user_becomes_verified_we_report_it() {
        // Given a room containing us and Bob
        let t = TestSetup::new_room_with_other_bob().await;

        // And Bob's identity is unpinned
        t.unpin_bob_with(IdentityChangeDataSet::key_query_with_identity_a()).await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob becomes verified
        t.verify_bob().await;

        // Then we were notified about the initial state of the room
        let change1 = assert_next_with_timeout!(stream);
        assert_eq!(change1[0].user_id, t.bob_user_id());
        assert_eq!(change1[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change1.len(), 1);

        // And the change when Bob became verified
        let change2 = assert_next_with_timeout!(stream);
        assert_eq!(change2[0].user_id, t.bob_user_id());
        assert_eq!(change2[0].changed_to, IdentityState::Verified);
        assert_eq!(change2.len(), 1);
    }

    #[async_test]
    async fn test_when_user_in_verification_violation_becomes_verified_we_report_it() {
        // Given a room containing us and Bob
        let t = TestSetup::new_room_with_other_bob().await;

        // And Bob is in verification violation
        t.verify_bob_with(
            IdentityChangeDataSet::key_query_with_identity_b(),
            IdentityChangeDataSet::master_signing_keys_b(),
            IdentityChangeDataSet::self_signing_keys_b(),
        )
        .await;
        t.unpin_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob becomes verified
        t.verify_bob().await;

        // Then we were notified about the initial state of the room
        let change1 = assert_next_with_timeout!(stream);
        assert_eq!(change1[0].user_id, t.bob_user_id());
        assert_eq!(change1[0].changed_to, IdentityState::VerificationViolation);
        assert_eq!(change1.len(), 1);

        // And the change when Bob became verified
        let change2 = assert_next_with_timeout!(stream);
        assert_eq!(change2[0].user_id, t.bob_user_id());
        assert_eq!(change2[0].changed_to, IdentityState::Verified);
        assert_eq!(change2.len(), 1);
    }

    #[async_test]
    async fn test_when_an_unpinned_user_joins_we_report_it() {
        // Given a room containing just us
        let mut t = TestSetup::new_just_me_room().await;

        // And Bob's identity is unpinned
        t.unpin_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob joins the room
        t.bob_joins().await;

        // Then we were notified about it
        let change = assert_next_with_timeout!(stream);
        assert_eq!(change[0].user_id, t.bob_user_id());
        assert_eq!(change[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change.len(), 1);
    }

    #[async_test]
    async fn test_when_an_verification_violating_user_joins_we_report_it() {
        // Given a room containing just us
        let mut t = TestSetup::new_just_me_room().await;

        // And Bob's identity is in verification violation
        t.verify_bob().await;
        t.unpin_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob joins the room
        t.bob_joins().await;

        // Then we were notified about it
        let change = assert_next_with_timeout!(stream);
        assert_eq!(change[0].user_id, t.bob_user_id());
        assert_eq!(change[0].changed_to, IdentityState::VerificationViolation);
        assert_eq!(change.len(), 1);
    }

    #[async_test]
    async fn test_when_a_verified_user_joins_we_dont_report_it() {
        // Given a room containing just us
        let mut t = TestSetup::new_just_me_room().await;

        // And Bob's identity is verified
        t.verify_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob joins the room
        t.bob_joins().await;

        // (Then becomes unpinned so we have something to report)
        t.unpin_bob().await;

        //// Then we were only notified about the unpin
        let change = assert_next_with_timeout!(stream);
        assert_eq!(change[0].user_id, t.bob_user_id());
        assert_eq!(change[0].changed_to, IdentityState::VerificationViolation);
        assert_eq!(change.len(), 1);
    }

    #[async_test]
    async fn test_when_a_pinned_user_joins_we_do_not_report() {
        // Given a room containing just us
        let mut t = TestSetup::new_just_me_room().await;

        // And Bob's identity is unpinned
        t.pin_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob joins the room
        t.bob_joins().await;

        // Then there is no notification
        tokio::time::sleep(Duration::from_millis(200)).await;
        let change = stream.next().now_or_never();
        assert!(change.is_none());
    }

    #[async_test]
    async fn test_when_an_unpinned_user_leaves_we_report_it() {
        // Given a room containing us and Bob
        let mut t = TestSetup::new_room_with_other_bob().await;

        // And Bob's identity is unpinned
        t.unpin_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob leaves the room
        t.bob_leaves().await;

        // Then we were notified about the initial state of the room
        let change1 = assert_next_with_timeout!(stream);
        assert_eq!(change1[0].user_id, t.bob_user_id());
        assert_eq!(change1[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change1.len(), 1);

        // And we were notified about the change when the user left
        let change2 = assert_next_with_timeout!(stream);
        // Note: the user left the room, but we see that as them "becoming
        // pinned" i.e. "you no longer need to notify about this user".
        assert_eq!(change2[0].user_id, t.bob_user_id());
        assert_eq!(change2[0].changed_to, IdentityState::Pinned);
        assert_eq!(change2.len(), 1);
    }

    #[async_test]
    async fn test_multiple_identity_changes_are_reported() {
        // Given a room containing just us
        let mut t = TestSetup::new_just_me_room().await;

        // And Bob's identity is unpinned
        t.unpin_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // NOTE: below we pull the changes out of the subscription after each
        // action. This makes sure that the identity changes and membership
        // changes are properly ordered. If we pull them out later, the identity
        // changes get shifted forward because they rely on less-complex async
        // stuff under the hood. Calling next_change ends up winding the async
        // machinery sufficiently that the membership change and any subsequent
        // events have fully completed.

        // When Bob joins the room ...
        t.bob_joins().await;
        let change1 = assert_next_with_timeout!(stream);

        // ... becomes pinned ...
        t.pin_bob().await;
        let change2 = assert_next_with_timeout!(stream);

        // ... leaves and joins again (ignored since they stay pinned) ...
        t.bob_leaves().await;
        t.bob_joins().await;

        // ... becomes unpinned ...
        t.unpin_bob().await;
        let change3 = assert_next_with_timeout!(stream);

        // ... and leaves.
        t.bob_leaves().await;
        let change4 = assert_next_with_timeout!(stream);

        assert_eq!(change1[0].user_id, t.bob_user_id());
        assert_eq!(change2[0].user_id, t.bob_user_id());
        assert_eq!(change3[0].user_id, t.bob_user_id());
        assert_eq!(change4[0].user_id, t.bob_user_id());

        assert_eq!(change1[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change2[0].changed_to, IdentityState::Pinned);
        assert_eq!(change3[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change4[0].changed_to, IdentityState::Pinned);

        assert_eq!(change1.len(), 1);
        assert_eq!(change2.len(), 1);
        assert_eq!(change3.len(), 1);
        assert_eq!(change4.len(), 1);
    }

    #[async_test]
    async fn test_when_an_unpinned_user_is_already_present_we_report_it_immediately() {
        // Given a room containing Bob, who is unpinned
        let t = TestSetup::new_room_with_other_bob().await;
        t.unpin_bob().await;

        // When we start listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // Then we were immediately notified about Bob being unpinned
        let change = assert_next_with_timeout!(stream);
        assert_eq!(change[0].user_id, t.bob_user_id());
        assert_eq!(change[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change.len(), 1);
    }

    #[async_test]
    async fn test_when_a_verified_user_is_already_present_we_dont_report_it() {
        // Given a room containing Bob, who is unpinned
        let t = TestSetup::new_room_with_other_bob().await;
        t.verify_bob().await;

        // When we start listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // (And we unpin so that something is available in the changes stream)
        t.unpin_bob().await;

        // Then we were only notified about the unpin, not being verified
        let next_change = assert_next_with_timeout!(stream);

        assert_eq!(next_change[0].user_id, t.bob_user_id());
        assert_eq!(next_change[0].changed_to, IdentityState::VerificationViolation);
        assert_eq!(next_change.len(), 1);
    }

    #[async_test]
    async fn test_no_sync_stream_never_requests_the_member_list() {
        // Given a room containing us and Bob whose stored member list is not
        // marked complete, so the syncing stream would request it
        let (t, server) = TestSetup::new_room_with_other_bob_unsynced().await;
        assert!(!t.room().are_members_synced());

        // And Bob's identity is unpinned
        t.unpin_bob().await;

        // When we listen through the stored-members stream
        let stream = t.subscribe_to_identity_status_changes_no_sync().await;
        pin_mut!(stream);

        // Then the initial state comes from the stored members
        let change1 = assert_next_with_timeout!(stream);
        assert_eq!(change1[0].user_id, t.bob_user_id());
        assert_eq!(change1[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change1.len(), 1);

        // And a later identity change is processed from the store
        t.pin_bob().await;
        let change2 = assert_next_with_timeout!(stream);
        assert_eq!(change2[0].user_id, t.bob_user_id());
        assert_eq!(change2[0].changed_to, IdentityState::Pinned);
        assert_eq!(change2.len(), 1);

        // And so is one after the list went from complete to incomplete (a
        // single change, from the pinned identity B to A)
        t.room().inner.mark_members_synced();
        t.room().inner.mark_members_missing();
        t.unpin_bob_with(IdentityChangeDataSet::key_query_with_identity_a()).await;
        let change3 = assert_next_with_timeout!(stream);
        assert_eq!(change3[0].user_id, t.bob_user_id());
        assert_eq!(change3[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change3.len(), 1);

        // And the member list was never requested or marked complete
        assert_eq!(members_requests(&server).await, 0);
        assert!(!t.room().are_members_synced());
    }

    #[async_test]
    async fn test_syncing_stream_requests_an_incomplete_member_list() {
        // Given the same room, whose member list is not marked complete
        let (t, server) = TestSetup::new_room_with_other_bob_unsynced().await;

        // When we listen through the syncing stream
        let _stream = t.subscribe_to_identity_status_changes().await;

        // Then it requested the member list: the count above sees requests
        assert!(members_requests(&server).await > 0);
    }

    #[async_test]
    async fn test_no_sync_stream_does_not_report_a_member_who_left_again() {
        // Given a room containing us and Bob, who is unpinned
        let mut t = TestSetup::new_room_with_other_bob().await;
        t.unpin_bob().await;

        // And we are listening through the stored-members stream
        let stream = t.subscribe_to_identity_status_changes_no_sync().await;
        pin_mut!(stream);
        let change1 = assert_next_with_timeout!(stream);
        assert_eq!(change1[0].user_id, t.bob_user_id());
        assert_eq!(change1[0].changed_to, IdentityState::PinViolation);

        // When Bob leaves, his warning is removed
        t.bob_leaves().await;
        let change2 = assert_next_with_timeout!(stream);
        assert_eq!(change2[0].user_id, t.bob_user_id());
        assert_eq!(change2[0].changed_to, IdentityState::Pinned);

        // And his stored membership is Leave, not absent
        let stored = t
            .room()
            .get_member_no_sync(t.bob_user_id())
            .await
            .expect("Should not fail to get member")
            .expect("A member who left stays stored");
        assert_eq!(*stored.membership(), MembershipState::Leave);

        // When his identity changes again while he is gone
        t.pin_bob().await;
        t.unpin_bob().await;

        // Then nothing is reported for him
        assert!(
            tokio::time::timeout(Duration::from_millis(200), stream.next()).await.is_err(),
            "a member who left is not warned about again"
        );

        // And when he joins again, the warning returns
        t.bob_joins().await;
        let change3 = assert_next_with_timeout!(stream);
        assert_eq!(change3[0].user_id, t.bob_user_id());
        assert_eq!(change3[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change3.len(), 1);
    }

    #[async_test]
    async fn test_no_sync_stream_reconciles_queued_pin_before_leave_or_ban() {
        for departure in [MembershipState::Leave, MembershipState::Ban] {
            queued_departure_is_reconciled(departure, false).await;
        }
    }

    #[async_test]
    async fn test_no_sync_stream_reconciles_queued_verification_before_leave_or_ban() {
        for departure in [MembershipState::Leave, MembershipState::Ban] {
            queued_departure_is_reconciled(departure, true).await;
        }
    }

    async fn queued_departure_is_reconciled(departure: MembershipState, verify: bool) {
        let (mut t, server) = TestSetup::new_room_with_other_bob_unsynced().await;
        t.unpin_bob().await;
        let stream = t.subscribe_to_identity_status_changes_no_sync().await;
        pin_mut!(stream);
        let initial = assert_next_with_timeout!(stream);
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].user_id, t.bob_user_id());
        assert_eq!(initial[0].changed_to, IdentityState::PinViolation);

        // Queue the real crypto update, then persist the departure, without
        // polling the stream between them. Either queue may be selected first.
        if verify {
            t.verify_bob_with(
                IdentityChangeDataSet::key_query_with_identity_b(),
                IdentityChangeDataSet::master_signing_keys_b(),
                IdentityChangeDataSet::self_signing_keys_b(),
            )
            .await;
        } else {
            t.pin_bob().await;
        }
        t.bob_membership_change(departure.clone()).await;
        let removed = assert_next_with_timeout!(stream);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].user_id, t.bob_user_id());
        assert_eq!(removed[0].changed_to, IdentityState::Pinned);

        // His actual identity now violates again while he is gone. A queued
        // Join followed by another persisted departure must not revive it.
        t.unpin_bob_with(IdentityChangeDataSet::key_query_with_identity_a()).await;
        t.bob_joins().await;
        t.bob_membership_change(departure).await;
        assert!(tokio::time::timeout(Duration::from_millis(200), stream.next()).await.is_err());

        // Rejoining really does warn again. A stale Leave in front of the
        // final Join must not remove that current active warning afterwards.
        t.bob_joins().await;
        t.bob_leaves().await;
        t.bob_joins().await;
        let rejoined = assert_next_with_timeout!(stream);
        assert_eq!(rejoined.len(), 1);
        assert_eq!(rejoined[0].user_id, t.bob_user_id());
        assert_eq!(
            rejoined[0].changed_to,
            if verify { IdentityState::VerificationViolation } else { IdentityState::PinViolation }
        );
        assert!(tokio::time::timeout(Duration::from_millis(200), stream.next()).await.is_err());
        assert_eq!(members_requests(&server).await, 0);
        assert!(!t.room().are_members_synced());
    }

    #[async_test]
    async fn test_no_sync_stream_ends_if_departure_rebuild_cannot_read_members() {
        let (mut t, server) = TestSetup::new_room_with_other_bob_unsynced().await;
        t.room().inner.mark_members_synced();
        t.unpin_bob().await;
        let stream = t.subscribe_to_identity_status_changes_no_sync().await;
        pin_mut!(stream);
        let initial = assert_next_with_timeout!(stream);
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].user_id, t.bob_user_id());
        assert_eq!(initial[0].changed_to, IdentityState::PinViolation);

        t.pin_bob().await;
        t.bob_leaves().await;
        // Bob's Leave is readable, so reconciliation detects the departure;
        // another active member's unreadable event makes the rebuild fail.
        t.make_stored_own_member_unreadable().await;
        let next = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("a failed rebuild ends the stream");
        assert!(next.is_none(), "an unreadable rebuild must not publish a removal/all-clear");
        assert_eq!(members_requests(&server).await, 0);
        assert!(t.room().are_members_synced());
    }

    #[async_test]
    async fn test_syncing_stream_keeps_queued_departure_and_historical_member_behavior() {
        let (mut t, server) = TestSetup::new_room_with_other_bob_unsynced().await;
        t.room().inner.mark_members_synced();
        t.unpin_bob().await;
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);
        let initial = assert_next_with_timeout!(stream);
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].changed_to, IdentityState::PinViolation);

        t.pin_bob().await;
        t.bob_leaves().await;
        let removed = assert_next_with_timeout!(stream);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].user_id, t.bob_user_id());
        assert_eq!(removed[0].changed_to, IdentityState::Pinned);

        // The existing default provider accepts historical stored members.
        // Keep that API's behavior, even though the opt-in stream excludes them.
        t.unpin_bob_with(IdentityChangeDataSet::key_query_with_identity_a()).await;
        let historical = assert_next_with_timeout!(stream);
        assert_eq!(historical.len(), 1);
        assert_eq!(historical[0].user_id, t.bob_user_id());
        assert_eq!(historical[0].changed_to, IdentityState::PinViolation);
        assert_eq!(members_requests(&server).await, 0);
    }

    #[test]
    fn test_stored_reconciliation_keeps_own_identity_excluded_from_publication() {
        use matrix_sdk_base::crypto::IdentityStatusChange;
        use ruma::user_id;

        let own_user_id = user_id!("@example:localhost");
        let other = IdentityStatusChange {
            user_id: user_id!("@bob:localhost").to_owned(),
            changed_to: IdentityState::Pinned,
        };
        let own = IdentityStatusChange {
            user_id: own_user_id.to_owned(),
            changed_to: IdentityState::PinViolation,
        };
        assert_eq!(
            super::filter_non_self(vec![own.clone(), other.clone()], own_user_id),
            vec![other]
        );
        assert!(super::filter_for_initial_update(vec![own], own_user_id).is_empty());
    }

    #[async_test]
    async fn test_no_sync_stream_does_not_start_from_an_unreadable_member_list() {
        // Given a room containing us and Bob whose stored member list is
        // marked complete, with its own recording mock server
        let (t, server) = TestSetup::new_room_with_other_bob_unsynced().await;
        t.room().inner.mark_members_synced();

        // And Bob's identity is pinned: the stored-members stream starts and
        // reports nobody (a known empty state, unlike the failure below)
        t.pin_bob().await;
        let known_empty = t.subscribe_to_identity_status_changes_no_sync().await;
        pin_mut!(known_empty);
        assert!(known_empty.next().now_or_never().is_none());

        // When Bob's stored member event can no longer be read from this
        // device's store
        t.make_stored_bob_member_unreadable().await;
        assert!(t.room().members_no_sync(RoomMemberships::JOIN).await.is_err());

        // Then the stored-members stream refuses to start: an unreadable
        // member list is not an empty one
        assert!(t.room().subscribe_to_identity_status_changes_no_sync().await.is_err());

        // And nothing asked the homeserver, and the list stays marked complete
        assert_eq!(members_requests(&server).await, 0);
        assert!(t.room().are_members_synced());
    }

    #[async_test]
    async fn test_no_sync_stream_ends_when_a_stored_member_cannot_be_read() {
        // Given a room containing us and Bob, who is unpinned, listened to
        // through the stored-members stream
        let (t, server) = TestSetup::new_room_with_other_bob_unsynced().await;
        t.room().inner.mark_members_synced();
        t.unpin_bob().await;
        let stream = t.subscribe_to_identity_status_changes_no_sync().await;
        pin_mut!(stream);
        let change1 = assert_next_with_timeout!(stream);
        assert_eq!(change1[0].user_id, t.bob_user_id());
        assert_eq!(change1[0].changed_to, IdentityState::PinViolation);

        // When his stored member event can no longer be read, and then his
        // identity changes
        t.make_stored_bob_member_unreadable().await;
        t.pin_bob().await;

        // Then the stream ends rather than dropping the change silently
        let next = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("the stream ends instead of waiting");
        assert!(next.is_none(), "a failed stored read is not reported as no change");

        // And nothing asked the homeserver
        assert_eq!(members_requests(&server).await, 0);
    }

    /// Requests for a room member list received by `server`.
    async fn members_requests(server: &wiremock::MockServer) -> usize {
        server
            .received_requests()
            .await
            .expect("Request recording is on")
            .iter()
            .filter(|request| request.url.path().ends_with("/members"))
            .count()
    }

    // TODO: I (andyb) haven't figured out how to test room membership changes
    // that affect our own user (they should not be shown). Specifically, I
    // haven't figure out how to get out own user into a non-pinned state.

    mod test_setup {
        use futures_core::Stream;
        use matrix_sdk_base::{
            RoomState, StateChanges,
            crypto::{
                IdentityStatusChange, OtherUserIdentity,
                testing::simulate_key_query_response_for_verification,
            },
        };
        use matrix_sdk_test::{
            DEFAULT_TEST_ROOM_ID, JoinedRoomBuilder, SyncResponseBuilder,
            event_factory::EventFactory, test_json,
            test_json::keys_query_sets::IdentityChangeDataSet,
        };
        use ruma::{
            OwnedUserId, TransactionId, UserId,
            api::client::keys::{get_keys, get_keys::v3::Response as KeyQueryResponse},
            events::{AnySyncStateEvent, StateEventType, room::member::MembershipState},
            owned_user_id,
            serde::Raw,
            user_id,
        };
        use serde_json::json;
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{header, method, path_regex},
        };

        use crate::{
            Client, Room, encryption::identities::UserIdentity, test_utils::logged_in_client,
        };

        /// Sets up a client and a room and allows changing user identities and
        /// room memberships. Note: most methods e.g. [`TestSetup::bob_user_id`]
        /// are talking about the OTHER user, not our own user. Only methods
        /// starting with `self_` are talking about this user.
        ///
        /// This user is called `@example:localhost` but is rarely used
        /// mentioned.
        ///
        /// The other user is called `@bob:localhost`.
        pub(super) struct TestSetup {
            client: Client,
            bob_user_id: OwnedUserId,
            sync_response_builder: SyncResponseBuilder,
            room: Room,
        }

        impl TestSetup {
            pub(super) async fn new_just_me_room() -> Self {
                let (client, user_id, mut sync_response_builder) = Self::init().await;
                let room = create_just_me_room(&client, &mut sync_response_builder).await;
                Self { client, bob_user_id: user_id, sync_response_builder, room }
            }

            pub(super) async fn new_room_with_other_bob() -> Self {
                let (client, bob_user_id, mut sync_response_builder) = Self::init().await;
                let room = create_room_with_other_member(
                    &mut sync_response_builder,
                    &client,
                    &bob_user_id,
                    true,
                )
                .await;
                Self { client, bob_user_id, sync_response_builder, room }
            }

            /// A room with Bob whose member list is not marked complete, and
            /// its own (not pooled) mock server, kept alive and recording every
            /// request so a test can count member-list requests.
            pub(super) async fn new_room_with_other_bob_unsynced() -> (Self, MockServer) {
                let server = MockServer::builder().start().await;
                mock_members_request(&server).await;
                mock_secret_storage_default_key(&server).await;
                let client = logged_in_client(Some(server.uri())).await;
                let (bob_user_id, mut sync_response_builder) = Self::init_client(&client).await;
                let room = create_room_with_other_member(
                    &mut sync_response_builder,
                    &client,
                    &bob_user_id,
                    false,
                )
                .await;
                (Self { client, bob_user_id, sync_response_builder, room }, server)
            }

            pub(super) fn bob_user_id(&self) -> &UserId {
                &self.bob_user_id
            }

            pub(super) fn room(&self) -> &Room {
                &self.room
            }

            pub(super) async fn pin_bob(&self) {
                if self.bob_user_identity().await.is_some() {
                    assert!(
                        !self.bob_is_pinned().await,
                        "pin_bob() called when the identity is already pinned!"
                    );

                    // Pin it
                    self.bob_user_identity()
                        .await
                        .expect("User should exist")
                        .pin()
                        .await
                        .expect("Should not fail to pin");
                } else {
                    // There was no existing identity. Set one. It will be
                    // pinned by default.
                    self.change_bob_identity(IdentityChangeDataSet::key_query_with_identity_a())
                        .await;
                }

                // Sanity check: they are pinned
                assert!(self.bob_is_pinned().await);
            }

            pub(super) async fn unpin_bob(&self) {
                self.unpin_bob_with(IdentityChangeDataSet::key_query_with_identity_b()).await;
            }

            pub(super) async fn unpin_bob_with(&self, requested: KeyQueryResponse) {
                fn master_key_json(key_query_response: &KeyQueryResponse) -> String {
                    serde_json::to_string(
                        key_query_response
                            .master_keys
                            .first_key_value()
                            .expect("Master key should have a value")
                            .1,
                    )
                    .expect("Should be able to serialise master key")
                }

                let a = IdentityChangeDataSet::key_query_with_identity_a();
                let b = IdentityChangeDataSet::key_query_with_identity_b();
                let requested_master_key = master_key_json(&requested);
                let a_master_key = master_key_json(&a);

                // Change/set their identity pin it, then change it again - this
                // will definitely unpin, even if the first identity we supply
                // is their very first, making them initially pinned.
                if requested_master_key == a_master_key {
                    self.change_bob_identity(b).await;
                    if !self.bob_is_pinned().await {
                        self.pin_bob().await;
                    }
                    self.change_bob_identity(a).await;
                } else {
                    self.change_bob_identity(a).await;
                    if !self.bob_is_pinned().await {
                        self.pin_bob().await;
                    }
                    self.change_bob_identity(b).await;
                }

                // Sanity: they are unpinned
                assert!(!self.bob_is_pinned().await);
            }

            pub(super) async fn verify_bob(&self) {
                self.verify_bob_with(
                    IdentityChangeDataSet::key_query_with_identity_a(),
                    IdentityChangeDataSet::master_signing_keys_a(),
                    IdentityChangeDataSet::self_signing_keys_a(),
                )
                .await;
            }

            pub(super) async fn verify_bob_with(
                &self,
                key_query: KeyQueryResponse,
                master_signing_key: serde_json::Value,
                self_signing_key: serde_json::Value,
            ) {
                // Make sure the requested identity is set
                self.change_bob_identity(key_query).await;

                let my_user_id = self.client.user_id().expect("I should have a user id");
                let my_identity = self
                    .client
                    .encryption()
                    .get_user_identity(my_user_id)
                    .await
                    .expect("Should not fail to get own user identity")
                    .expect("Should have an own user identity")
                    .underlying_identity()
                    .own()
                    .expect("Our own identity should be of type Own");

                // Get the request
                let signature_upload_request = self
                    .bob_crypto_other_identity()
                    .await
                    .verify()
                    .await
                    .expect("Should be able to verify other identity");

                let verification_response = simulate_key_query_response_for_verification(
                    signature_upload_request,
                    my_identity,
                    my_user_id,
                    self.bob_user_id(),
                    master_signing_key,
                    self_signing_key,
                );

                // Receive the response into our client
                self.client
                    .mark_request_as_sent(&TransactionId::new(), &verification_response)
                    .await
                    .unwrap();

                // Sanity: they are verified
                assert!(self.bob_is_verified().await);
            }

            pub(super) async fn bob_joins(&mut self) {
                self.bob_membership_change(MembershipState::Join).await;
            }

            pub(super) async fn bob_leaves(&mut self) {
                self.bob_membership_change(MembershipState::Leave).await;
            }

            pub(super) async fn subscribe_to_identity_status_changes(
                &self,
            ) -> impl Stream<Item = Vec<IdentityStatusChange>> + use<> {
                self.room
                    .subscribe_to_identity_status_changes()
                    .await
                    .expect("Should be able to subscribe")
            }

            pub(super) async fn subscribe_to_identity_status_changes_no_sync(
                &self,
            ) -> impl Stream<Item = Vec<IdentityStatusChange>> + use<> {
                self.room
                    .subscribe_to_identity_status_changes_no_sync()
                    .await
                    .expect("Should be able to subscribe")
            }

            /// Replace Bob's member event in this device's state store with a
            /// copy that cannot be read (it has no `membership`). The store
            /// keeps his indexed Join membership, so a later local read of the
            /// member list or of Bob fails. Nothing is synced or sent.
            pub(super) async fn make_stored_bob_member_unreadable(&self) {
                self.make_stored_member_unreadable(&self.bob_user_id).await;
            }

            pub(super) async fn make_stored_own_member_unreadable(&self) {
                self.make_stored_member_unreadable(self.room.own_user_id()).await;
            }

            async fn make_stored_member_unreadable(&self, user_id: &UserId) {
                let unreadable: Raw<AnySyncStateEvent> = Raw::from_json_string(
                    json!({
                        "type": "m.room.member",
                        "state_key": user_id,
                        "sender": user_id,
                        "event_id": "$unreadable-member",
                        "origin_server_ts": 1,
                        "content": {},
                    })
                    .to_string(),
                )
                .expect("Should be valid JSON");
                let mut changes = StateChanges::default();
                changes
                    .state
                    .entry(self.room.room_id().to_owned())
                    .or_default()
                    .entry(StateEventType::RoomMember)
                    .or_default()
                    .insert(user_id.to_string(), unreadable);
                self.client
                    .state_store()
                    .save_changes(&changes)
                    .await
                    .expect("Should save to this device's store");
            }

            async fn init() -> (Client, OwnedUserId, SyncResponseBuilder) {
                let (client, _server) = create_client_and_server().await;
                let (bob_user_id, sync_response_builder) = Self::init_client(&client).await;
                (client, bob_user_id, sync_response_builder)
            }

            async fn init_client(client: &Client) -> (OwnedUserId, SyncResponseBuilder) {
                // Ensure our user has cross-signing keys etc.
                client
                    .olm_machine()
                    .await
                    .as_ref()
                    .expect("We should have an Olm machine")
                    .bootstrap_cross_signing(true)
                    .await
                    .expect("Should be able to bootstrap cross-signing");

                // Note: if you change the user_id, you will need to change lots
                // of hard-coded stuff inside IdentityChangeDataSet
                let bob_user_id = owned_user_id!("@bob:localhost");

                let sync_response_builder = SyncResponseBuilder::default();

                (bob_user_id, sync_response_builder)
            }

            async fn change_bob_identity(
                &self,
                key_query_response: get_keys::v3::Response,
            ) -> OtherUserIdentity {
                self.client
                    .mark_request_as_sent(&TransactionId::new(), &key_query_response)
                    .await
                    .expect("Should not fail to send identity changes");

                self.bob_crypto_other_identity().await
            }

            pub(super) async fn bob_membership_change(&mut self, new_state: MembershipState) {
                let f = EventFactory::new().sender(user_id!("@example:localhost"));
                let sync_response = self
                    .sync_response_builder
                    .add_joined_room(
                        JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID).add_state_event(
                            f.member(&self.bob_user_id).membership(new_state.clone()),
                        ),
                    )
                    .build_sync_response();
                self.room.client.process_sync(sync_response).await.unwrap();

                // Make sure the membership stuck as expected
                let m = self
                    .room
                    .get_member_no_sync(&self.bob_user_id)
                    .await
                    .expect("Should not fail to get member");

                match (&new_state, m) {
                    (MembershipState::Leave, None) => {}
                    (_, None) => {
                        panic!("Member should exist")
                    }
                    (_, Some(m)) => {
                        assert_eq!(*m.membership(), new_state);
                    }
                }
            }

            async fn bob_is_pinned(&self) -> bool {
                !self.bob_crypto_other_identity().await.identity_needs_user_approval()
            }

            async fn bob_is_verified(&self) -> bool {
                self.bob_crypto_other_identity().await.is_verified()
            }

            async fn bob_crypto_other_identity(&self) -> OtherUserIdentity {
                self.bob_user_identity()
                    .await
                    .expect("User identity should exist")
                    .underlying_identity()
                    .other()
                    .expect("Identity should be Other, not Own")
            }

            async fn bob_user_identity(&self) -> Option<UserIdentity> {
                self.client
                    .encryption()
                    .get_user_identity(&self.bob_user_id)
                    .await
                    .expect("Should not fail to get user identity")
            }
        }

        async fn create_just_me_room(
            client: &Client,
            sync_response_builder: &mut SyncResponseBuilder,
        ) -> Room {
            let f = EventFactory::new().sender(user_id!("@example:localhost"));
            let create_room_sync_response = sync_response_builder
                .add_joined_room(JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID).add_state_event(
                    f.member(user_id!("@example:localhost")).display_name("example"),
                ))
                .build_sync_response();
            client.process_sync(create_room_sync_response).await.unwrap();
            let room = client.get_room(&DEFAULT_TEST_ROOM_ID).expect("Room should exist");
            assert_eq!(room.state(), RoomState::Joined);
            room
        }

        async fn create_room_with_other_member(
            builder: &mut SyncResponseBuilder,
            client: &Client,
            other_user_id: &UserId,
            members_synced: bool,
        ) -> Room {
            let f = EventFactory::new().sender(user_id!("@example:localhost"));
            let create_room_sync_response = builder
                .add_joined_room(
                    JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID)
                        .add_state_event(
                            f.member(user_id!("@example:localhost")).display_name("example"),
                        )
                        .add_state_event(f.member(other_user_id).membership(MembershipState::Join)),
                )
                .build_sync_response();
            client.process_sync(create_room_sync_response).await.unwrap();
            let room = client.get_room(&DEFAULT_TEST_ROOM_ID).expect("Room should exist");
            if members_synced {
                room.inner.mark_members_synced();
            }

            assert_eq!(room.state(), RoomState::Joined);
            assert_eq!(
                *room
                    .get_member_no_sync(other_user_id)
                    .await
                    .expect("Should not fail to get member")
                    .expect("Member should exist")
                    .membership(),
                MembershipState::Join
            );
            room
        }

        async fn create_client_and_server() -> (Client, MockServer) {
            let server = MockServer::start().await;
            mock_members_request(&server).await;
            mock_secret_storage_default_key(&server).await;
            let client = logged_in_client(Some(server.uri())).await;
            (client, server)
        }

        async fn mock_members_request(server: &MockServer) {
            Mock::given(method("GET"))
                .and(path_regex(r"^/_matrix/client/r0/rooms/.*/members"))
                .and(header("authorization", "Bearer 1234"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(&*test_json::members::MEMBERS),
                )
                .mount(server)
                .await;
        }

        async fn mock_secret_storage_default_key(server: &MockServer) {
            Mock::given(method("GET"))
                .and(path_regex(
                    r"^/_matrix/client/r0/user/.*/account_data/m.secret_storage.default_key",
                ))
                .and(header("authorization", "Bearer 1234"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
                .mount(server)
                .await;
        }
    }
}
