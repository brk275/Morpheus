// Copyright 2026 Element Creations Ltd.
//
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Element-Commercial
// Please see LICENSE in the repository root for full details.

//! In-memory RTC session membership model.
//!
//! This module stores the current participant view for one `(room, slot)` and
//! applies joined/left transitions from domain membership events routed to it
//! by its [`BaseRtcRoom`](crate::BaseRtcRoom), the only thing that creates one.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch};

use crate::encryption::types::ReceivedEncryptionKey;
use crate::encryption::{EncryptionKeySignalHandler, EncryptionManager, RtcIdentityMapper};
use crate::error::{CommandError, JoinError, LeaveError};
use crate::executor::{AbortHandle, AbortOnDrop};
use crate::host::backend::MatrixBackend;
use crate::host::event::EventOrigin;
use crate::join::{JoinSessionParams, LeaveSessionParams, TransportIntent};
use crate::membership_listener::MembershipScope;
use crate::own_membership::{
    MembershipTimings, OwnMembershipMachine, OwnMembershipState, transport_to_json,
};
use crate::slot::{RoomEncryption, SlotState};
use crate::transport::{MemberTransports, RtcTransport};
use crate::upkeep::{self, SessionUpkeep};

#[allow(unused_imports)]
use log::*;

/// What a session knows about the room state governing its slot.
///
/// MSC4143 makes an open `m.rtc.slot` a precondition for anyone being joined,
/// so "no open slot" and "nobody has told us about the slot" have to be
/// distinguished: the first means every member is left, the second means the
/// condition cannot be evaluated yet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SlotKnowledge {
    /// No room state has been supplied for this room, so the slot condition is
    /// not enforced. Hosts opt in by calling
    /// [`BaseRtcRoom::on_slots_received`](crate::BaseRtcRoom::on_slots_received).
    #[default]
    Unsupplied,
    /// Room state has been supplied; this is the slot's resolved state. A slot
    /// with no state event in the room resolves to [`SlotState::Closed`].
    Known(SlotState),
}

fn sticky_keys(members: &[JoinedMembership]) -> Vec<&str> {
    members
        .iter()
        .map(|member| member.sticky_key.as_str())
        .collect()
}

fn difference<'a>(from: &[&'a str], without: &[&str]) -> Vec<&'a str> {
    from.iter()
        .filter(|key| !without.contains(key))
        .copied()
        .collect()
}

fn describe_exclusions(excluded: &[(&str, JoinCondition)]) -> String {
    excluded
        .iter()
        .map(|(sticky_key, condition)| format!("{sticky_key}={condition:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Why a candidate member event is, or is not, projected as joined.
///
/// A plain `bool` here made the most confusing failure in the whole SDK
/// invisible: a member vanishing from the joined memberships because of room state they
/// have nothing to do with. Carrying the reason costs nothing and turns that
/// into one readable log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JoinCondition {
    /// Every MSC4143 condition this session can evaluate is satisfied.
    Joined,
    /// The slot is closed, so nobody is joined to it.
    SlotClosed,
    /// MSC4143 requires `m.rtc.member` to be encrypted in an encrypted room,
    /// and one that is not "MUST be considered left".
    UnencryptedInEncryptedRoom,
    /// The sender is no longer joined to the room.
    SenderNotInRoom,
    /// A previous participation of *this* device, still sticky under an older
    /// `member_id`. MSC4143 mints a fresh `member.id` per join, so this is us one
    /// call ago, not a peer.
    SupersededOwnParticipation,
}

impl JoinCondition {
    fn is_joined(self) -> bool {
        matches!(self, JoinCondition::Joined)
    }
}

/// Who we are in the session's current join, for as long as we are joined.
///
/// Kept next to the membership machine so the join conditions can recognise our
/// own superseded participations. The `member_id` alone is not enough: telling a
/// stale participation of *this* device apart from another device of the same
/// user needs the device too.
#[derive(Clone, Debug)]
struct OwnParticipation {
    user_id: String,
    device_id: String,
    member_id: String,
}

const PEER_RENEWAL_TOMBSTONES: usize = 256;
const PEER_RENEWAL_ID_BYTES: usize = 255;

/// Per-own-join logical identities, including peers that have disappeared. Event
/// IDs, timestamps and transports deliberately do not form part of this key:
/// refreshing ordinary room content must never create a refresh feedback loop.
/// At capacity the optimization stays disabled until our next join, rather than
/// evicting identities and admitting their replays.
#[derive(Default)]
struct PeerRenewalLedger {
    active: bool,
    seen: HashSet<(String, String, String)>,
    // A separate finite budget for device hints. At capacity retain tombstones
    // and ignore new hints; ordinary membership-based renewal stays enabled.
    seen_devices: HashSet<(String, String)>,
}

#[derive(Debug, PartialEq, Eq)]
enum PeerRenewalAction {
    None,
    Request,
    Disable,
}

impl PeerRenewalLedger {
    fn observe_devices(
        &mut self,
        devices: &[(String, String)],
        own: &OwnParticipation,
        slot: &SlotKnowledge,
        room_members: Option<&HashSet<String>>,
        encryption: RoomEncryption,
    ) -> bool {
        if !self.active
            || encryption != RoomEncryption::Encrypted
            || !matches!(slot, SlotKnowledge::Known(state) if state.is_open())
        {
            return false;
        }
        let Some(room_members) = room_members else {
            return false;
        };
        let mut added = false;
        for (user, device) in devices.iter().take(PEER_RENEWAL_TOMBSTONES) {
            if !room_members.contains(user)
                || (user == &own.user_id && device == &own.device_id)
                || [user, device].into_iter().any(|id| {
                    id.is_empty()
                        || id.len() > PEER_RENEWAL_ID_BYTES
                        || id.chars().any(char::is_control)
                })
            {
                continue;
            }
            let identity = (user.clone(), device.clone());
            if self.seen_devices.contains(&identity) {
                continue;
            }
            if self.seen_devices.len() == PEER_RENEWAL_TOMBSTONES {
                break;
            }
            self.seen_devices.insert(identity);
            added = true;
        }
        added
    }

    fn observe(
        &mut self,
        members: &[JoinedMembership],
        own: &OwnParticipation,
        slot: &SlotKnowledge,
        room_members: Option<&HashSet<String>>,
        encryption: RoomEncryption,
        baseline: bool,
    ) -> PeerRenewalAction {
        if !self.active
            || encryption != RoomEncryption::Encrypted
            || !matches!(slot, SlotKnowledge::Known(state) if state.is_open())
        {
            return PeerRenewalAction::None;
        }
        let Some(room_members) = room_members else {
            return PeerRenewalAction::None;
        };
        let mut added = false;
        for member in members {
            let EventOrigin::Encrypted {
                sender_device_id: Some(device),
            } = &member.origin
            else {
                continue;
            };
            if !room_members.contains(&member.sender)
                || (member.sender == own.user_id && device == &own.device_id)
                || member.member_id != member.sticky_key
                || [&member.sender, device, &member.member_id]
                    .into_iter()
                    .any(|id| {
                        id.is_empty()
                            || id.len() > PEER_RENEWAL_ID_BYTES
                            || id.chars().any(char::is_control)
                    })
            {
                continue;
            }
            let identity = (
                member.sender.clone(),
                device.clone(),
                member.member_id.clone(),
            );
            if self.seen.contains(&identity) {
                continue;
            }
            if self.seen.len() == PEER_RENEWAL_TOMBSTONES {
                self.active = false;
                return PeerRenewalAction::Disable;
            }
            self.seen.insert(identity);
            added = true;
        }
        if added && !baseline {
            PeerRenewalAction::Request
        } else {
            PeerRenewalAction::None
        }
    }
}

/// Buffer of [`SlotSession::subscribe_auto_leaves`]. One auto-leave ends a
/// join, so a subscriber only falls behind across several rejoins.
const AUTO_LEAVES_CAPACITY: usize = 4;

/// Per-session MatrixRTC state machine and membership store.
pub struct SlotSession<T: MatrixBackend> {
    /// Member events that are join-shaped and still sticky. These are
    /// candidates only: the remaining MSC4143 join conditions depend on room
    /// state, which can change under them at any time.
    candidates: Vec<JoinedMembership>,
    /// The candidates that currently satisfy every join condition. This is what
    /// gets published and what the encryption manager distributes keys to.
    members: Vec<JoinedMembership>,
    /// The slot this session's members are joined to.
    slot: SlotKnowledge,
    /// Users currently joined to the room, when the host supplies them; `None`
    /// leaves the room-membership condition unenforced.
    room_members: Option<HashSet<String>>,
    /// Whether the room is encrypted, which decides whether member events are
    /// required to be encrypted.
    room_encryption: RoomEncryption,
    membership_snapshots_tx: watch::Sender<Vec<JoinedMembership>>,
    /// Announces every leave this session makes without the host asking. See
    /// [`SlotSession::subscribe_auto_leaves`].
    auto_leaves_tx: broadcast::Sender<LeaveReason>,
    /// Command sender for sending events to the Matrix room.
    backend: Option<Arc<T>>,
    /// Machine for managing our own membership lifecycle (join/leave/keep-alive).
    /// Shared with the upkeep, which ticks it.
    own_membership_machine: Option<Arc<OwnMembershipMachine<T>>>,
    /// The task keeping our membership alive and rotating keys on time, from
    /// join until leave; dropping the session stops it.
    upkeep: Option<AbortOnDrop<()>>,
    /// Identity of the current join, or `None` while not joined.
    own_participation: Option<OwnParticipation>,
    peer_renewals: PeerRenewalLedger,
    /// Encryption manager for key distribution and management.
    encryption_manager: Option<EncryptionManager<T>>,
    room_id: String,
    slot_id: String,
    membership_scope: MembershipScope,
    /// `room_id/slot_id`, prefixed to this session's log lines, so every line
    /// from a multi-call client is attributable. Pre-formatted because it is
    /// used on paths that run per event.
    log_tag: String,
}

impl<T: MatrixBackend> Clone for SlotSession<T> {
    fn clone(&self) -> Self {
        Self {
            candidates: self.candidates.clone(),
            members: self.members.clone(),
            slot: self.slot.clone(),
            room_members: self.room_members.clone(),
            room_encryption: self.room_encryption,
            membership_snapshots_tx: self.membership_snapshots_tx.clone(),
            auto_leaves_tx: self.auto_leaves_tx.clone(),
            backend: self.backend.clone(),
            own_membership_machine: None, // Don't clone the machine - it's not cloneable
            upkeep: None,
            encryption_manager: None, // Don't clone the encryption manager
            own_participation: None,  // A clone holds no machine, so it is not joined
            peer_renewals: PeerRenewalLedger::default(),
            room_id: self.room_id.clone(),
            slot_id: self.slot_id.clone(),
            membership_scope: self.membership_scope.clone(), // Shares the watch, so shares its listeners
            log_tag: self.log_tag.clone(),
        }
    }
}

impl<T: MatrixBackend + 'static> SlotSession<T> {
    /// Creates an empty session for one `(room_id, slot_id)`; called by the
    /// room on the slot's first use.
    pub(crate) fn new(
        room_id: String,
        slot_id: String,
        backend: Option<Arc<T>>,
        membership_scope: MembershipScope,
    ) -> Self {
        let (membership_snapshots_tx, _membership_snapshots_rx) = watch::channel(Vec::new());
        let log_tag = format!("{room_id}/{slot_id}");

        Self {
            candidates: Vec::new(),
            members: Vec::new(),
            slot: SlotKnowledge::default(),
            room_members: None,
            room_encryption: RoomEncryption::default(),
            membership_snapshots_tx,
            auto_leaves_tx: broadcast::channel(AUTO_LEAVES_CAPACITY).0,
            backend,
            own_membership_machine: None,
            upkeep: None,
            encryption_manager: None,
            own_participation: None,
            peer_renewals: PeerRenewalLedger::default(),
            room_id,
            slot_id,
            membership_scope,
            log_tag,
        }
    }

    /// The joined set as last published.
    pub(crate) fn members(&self) -> &[JoinedMembership] {
        &self.members
    }

    /// Sets the backend for this session.
    pub fn set_backend(&mut self, backend: Arc<T>) {
        self.backend = Some(backend);
    }

    /// Returns true if this session has a backend configured.
    pub fn has_backend(&self) -> bool {
        self.backend.is_some()
    }

    /// Registers a handler that receives media key material signalled by this
    /// session's encryption manager.
    ///
    /// Returns `false` if the session has not joined yet (no encryption manager
    /// exists). Call after [`SlotSession::join`].
    pub fn set_encryption_signal_handler(
        &mut self,
        handler: Arc<dyn EncryptionKeySignalHandler>,
    ) -> bool {
        match &mut self.encryption_manager {
            Some(manager) => {
                manager.set_signal_handler(handler);
                // Install with this live borrowed view while the session is
                // exclusively held; no earlier watch/mapper view can win.
                manager.on_membership_snapshot(&self.members);
                true
            }
            None => false,
        }
    }

    /// Re-signals every key already held to the installed signal handler.
    ///
    /// Call after both the handler and the identity mapper are in place; see
    /// [`EncryptionManager::replay_keys_to_handler`]. Returns `false` if the
    /// session has no encryption manager.
    pub async fn replay_encryption_keys(&self) -> bool {
        match &self.encryption_manager {
            Some(manager) => {
                manager.replay_keys_to_handler().await;
                true
            }
            None => false,
        }
    }

    /// When a coalesced key rotation falls due, if one is owed.
    ///
    /// `None` while nothing is owed, or when not joined.
    pub fn key_rotation_due_at_ms(&self) -> Option<u64> {
        self.encryption_manager
            .as_ref()
            .and_then(|manager| manager.rotation_due_at_ms())
    }

    /// Follows [`Self::key_rotation_due_at_ms`], so a scheduler can wake at the
    /// deadline. `None` when not joined.
    pub fn subscribe_key_rotation_due(&self) -> Option<watch::Receiver<Option<u64>>> {
        self.encryption_manager
            .as_ref()
            .map(EncryptionManager::subscribe_rotation_due)
    }

    /// Performs a key rotation coalesced into a fresh key's window, if one is owed
    /// and due.
    ///
    /// Returns `false` if the session has not joined.
    pub async fn flush_due_key_rotation(&self) -> bool {
        match &self.encryption_manager {
            Some(manager) => {
                if let Err(error) = manager.flush_due_rotation().await {
                    log::warn!(
                        "[{}] a deferred key rotation failed: {error:?}",
                        self.log_tag,
                    );
                }
                true
            }
            None => false,
        }
    }

    /// Installs the identity mapper used to derive the RTC-backend participant
    /// identity carried in signalled key material (see [`RtcIdentityMapper`]).
    ///
    /// Returns `false` if the session has not joined yet. Call after
    /// [`SlotSession::join`].
    pub fn set_encryption_identity_mapper(&mut self, mapper: RtcIdentityMapper) -> bool {
        match &mut self.encryption_manager {
            Some(manager) => {
                manager.set_identity_mapper(mapper);
                true
            }
            None => false,
        }
    }

    /// Feeds a media encryption key received from a peer (MSC4143 to-device
    /// message) into this session's encryption manager.
    ///
    /// The key is only used if it passes the MSC4143 checks; see
    /// [`EncryptionManager::receive_key`]. A no-op if the session has not
    /// joined yet (no encryption manager).
    pub async fn receive_encryption_key(
        &self,
        received: ReceivedEncryptionKey,
    ) -> Result<(), CommandError> {
        match &self.encryption_manager {
            Some(manager) => manager.receive_key(received).await,
            None => Ok(()),
        }
    }

    /// Joins this RTC session with the given parameters.
    ///
    /// This sends a membership event to the Matrix room via the backend,
    /// and starts the keep-alive mechanism to ensure proper cleanup.
    ///
    /// The dead man's switch strategy is used:
    /// 1. Schedule delayed leave event FIRST (safety net) - **awaited**
    /// 2. Send join membership event - **awaited**
    /// 3. Heartbeat will restart the delayed leave periodically
    ///
    /// The async design ensures the delayed leave is scheduled before the join event is sent.
    ///
    /// # Arguments
    ///
    /// * `params` - The slot, the application, the transport, and any
    ///   overrides; the user and device are the backend's.
    ///
    /// # Returns
    ///
    /// Returns the event id of the membership event this join sent, which an
    /// application relates its own events to (a call's MSC4075 notification).
    /// Returns `Err(JoinError)` if validation fails, backend not configured, or commands fail.
    pub async fn join(&mut self, params: JoinSessionParams) -> Result<String, JoinError> {
        params.validate().map_err(|missing| {
            log::warn!("[{}] join rejected: missing {missing}", self.log_tag);
            JoinError::MissingParameter(missing)
        })?;

        let backend = self.backend.as_ref().ok_or_else(|| {
            log::warn!(
                "[{}] join rejected: no backend configured — the host must call \
                 set_backend before joining",
                self.log_tag,
            );
            JoinError::CommandError(CommandError::from_message("no backend configured"))
        })?;

        let membership_id = params.membership_id();
        let user_id = backend.own_user_id();
        let device_id = backend.own_device_id();
        let transport = params
            .transport
            .clone()
            .expect("validate() refuses a join without a transport");

        // Check if already joined with this membership
        if self
            .own_membership_machine
            .as_ref()
            .is_some_and(|machine| machine.sticky_key() == membership_id)
        {
            log::warn!("[{}] already joined as {membership_id}", self.log_tag);
            return Err(JoinError::AlreadyJoined(membership_id));
        }

        log::info!(
            "[{}] joining as {membership_id} ({user_id}/{device_id}, transport {transport:?})",
            self.log_tag,
        );

        let transports = match &transport {
            TransportIntent::Publish(transport) => MemberTransports::publishing(
                serde_json::from_value(transport_to_json(transport))
                    .expect("a transport always serializes to an object with a type"),
            ),
            TransportIntent::ReceiveOnly { can_subscribe } => MemberTransports {
                published: Vec::new(),
                can_subscribe: can_subscribe.clone(),
            },
        };
        let keep_alive_interval = Duration::from_millis(params.effective_keep_alive_interval_ms());
        let machine = OwnMembershipMachine::new(
            backend.clone(),
            self.room_id.clone(),
            self.slot_id.clone(),
            membership_id.clone(),
            params.application.clone(),
            MembershipTimings {
                keep_alive_timeout_ms: params.effective_keep_alive_timeout_ms(),
                sticky_duration_ms: params.effective_sticky_duration_ms(),
                degraded_lifetime_ms: params.effective_degraded_lifetime_ms(),
            },
        );

        // Use the machine to join (async, awaits both delayed leave scheduling and join event)
        let membership_event_id = machine.join(transports).await?;

        // Store the machine
        self.own_membership_machine = Some(Arc::new(machine));
        self.own_participation = Some(OwnParticipation {
            user_id: user_id.clone(),
            device_id: device_id.clone(),
            member_id: membership_id.clone(),
        });

        // The initial encrypted send already served currently eligible peers.
        // Seed their tombstones once; later arrivals request one heartbeat send.
        self.peer_renewals = PeerRenewalLedger {
            active: true,
            ..PeerRenewalLedger::default()
        };
        if let Some(own) = &self.own_participation {
            let action = self.peer_renewals.observe(
                &self.members,
                own,
                &self.slot,
                self.room_members.as_ref(),
                self.room_encryption,
                true,
            );
            if action == PeerRenewalAction::Disable {
                self.own_membership_machine
                    .as_ref()
                    .unwrap()
                    .disable_peer_renewal();
            }
        }

        // Name ourselves in the log tag from here on. Until now `room/slot` was
        // enough, because one process meant one session per slot. A host that
        // runs several participations of the same slot in one process — the load
        // generator does, with ten — otherwise emits every session's lines under
        // an identical prefix, and the interleaving cannot be untangled even in
        // principle: "membership changed" from one device sits next to
        // "candidate added" from another, and any conclusion drawn is a guess.
        self.log_tag = format!("{}/{device_id}", self.log_tag);

        // Create the encryption manager
        // We need a closure that can access self.members
        // Since we can't capture self by reference in an Arc closure, we'll use a different approach
        // For now, we'll create a simple closure that clones the members vector
        let get_memberships_for_encryption = {
            let members_tx = self.membership_snapshots_tx.clone();
            move || members_tx.borrow().clone()
        };

        let mut encryption_config = params.effective_encryption_config();
        if let Some(negotiated) = self.negotiated_encryption() {
            // The slot decides whether RTC data is encrypted; the local flag only
            // applies where no slot state has been supplied to negotiate from.
            encryption_config.manage_media_keys = negotiated;
        }

        let mut encryption_manager = EncryptionManager::new(
            backend.clone(),
            user_id,
            device_id,
            membership_id.clone(),
            self.room_id.clone(),
            self.slot_id.clone(),
            get_memberships_for_encryption,
        );
        let manage_media_keys = encryption_config.manage_media_keys;
        encryption_manager.set_config(encryption_config);

        // Start the encryption manager (creates first key)
        encryption_manager.join().await.map_err(|e| {
            log::warn!(
                "[{}] encryption manager failed to start: {e:?}",
                self.log_tag
            );
            JoinError::CommandError(CommandError::from_message(format!(
                "failed to start encryption manager: {:?}",
                e
            )))
        })?;

        // Store the encryption manager
        self.encryption_manager = Some(encryption_manager);

        // Re-evaluate the joined memberships: we now have an `own_participation`, so any
        // still-sticky participation of this device from a previous call stops
        // counting as a member. This also publishes the joined memberships the media layer's
        // engine will boot from.
        self.refresh().await;

        // Drive the first distribution from the join itself, unconditionally.
        //
        // `on_memberships_update` is otherwise only reachable through
        // `refresh()`, which returns early when the joined memberships are unchanged — and a
        // session outlives a `leave()`, so a second join in the same process
        // starts with the previous call's joined memberships already in place and has no
        // change to ride on. Without this, the first call in a process
        // distributes its key (the peer's arrival changed the joined memberships) and every
        // later one silently distributes nothing, leaving peers at
        // `MISSING_KEY` for the whole call.
        //
        // A failure here is logged rather than propagated: an unreachable peer
        // must not fail the join, exactly as it does not fail a rollout
        // triggered by a membership change.
        if let Some(encryption_manager) = self.encryption_manager.as_ref()
            && let Err(error) = encryption_manager.on_memberships_update().await
        {
            log::warn!(
                "[{}] the join's first key distribution failed: {error:?}",
                self.log_tag,
            );
        }

        self.upkeep = self.own_membership_machine.clone().and_then(|machine| {
            SessionUpkeep {
                machine,
                encryption: self.encryption_manager.clone(),
                interval: keep_alive_interval,
                log_tag: self.log_tag.clone(),
            }
            .spawn()
        });

        log::info!(
            "[{}] joined as {membership_id} (media keys {})",
            self.log_tag,
            if manage_media_keys { "managed" } else { "off" },
        );

        Ok(membership_event_id)
    }

    /// Leaves this RTC session.
    ///
    /// This sends a left membership event and cancels any active keep-alive delayed event.
    ///
    /// # What survives
    ///
    /// Only *own-participation* state is dropped: the membership machine, the
    /// encryption manager and our identity. Room state (`slot`, `room_members`,
    /// `room_encryption`) and peer `candidates` are kept, and the session itself
    /// stays registered with its room. Three reasons, all load-bearing:
    ///
    /// - Hosts feed sticky *deltas*. An unchanged peer membership is never
    ///   re-delivered, so a session reset to pristine would rejoin into an empty
    ///   joined memberships and never learn about the members already in the call.
    /// - A host that has hung up may still want the joined memberships — "3 people are in
    ///   this call" outlives our own participation.
    /// - The media session is torn down separately, with no ordering guarantee
    ///   relative to this call. Dropping the membership channel here would make a
    ///   still-running engine stop tracking membership mid-call.
    ///
    /// Because the session outlives a leave, [`Self::join`] must not depend on the
    /// joined memberships changing after it returns — it drives the first key distribution
    /// itself.
    ///
    /// # Arguments
    ///
    /// * `params` - The leave parameters including optional disconnect reason.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if the leave completed successfully.
    /// Returns `Err(LeaveError)` if not joined, backend is not configured, or commands fail.
    /// A failed send still ends the join here, since the machine is gone and
    /// cannot retry; the delayed leave removes the membership.
    pub async fn leave(&mut self, params: LeaveSessionParams) -> Result<(), LeaveError> {
        // Check if we have a membership machine (i.e., we've joined)
        let machine = self.own_membership_machine.take().ok_or_else(|| {
            log::warn!("[{}] leave rejected: not joined", self.log_tag);
            LeaveError::NotJoined
        })?;
        self.peer_renewals = PeerRenewalLedger::default();
        // A tick in flight finishes first (the machine serialises the two),
        // and finds nothing joined afterwards.
        let upkeep = self.upkeep.take();
        if let Some(encryption) = self.encryption_manager.as_ref() {
            encryption.stop_key_retries();
        }

        log::info!(
            "[{}] leaving as {} (reason {:?})",
            self.log_tag,
            machine.sticky_key(),
            params.leave_reason,
        );

        // Use the machine to leave (async, awaits both leave event and delayed event cancellation)
        let sent = machine
            .leave(params.leave_reason.clone())
            .await
            .inspect_err(|error| log::warn!("[{}] leave failed: {error}", self.log_tag));

        // Clean up the encryption manager
        if let Some(encryption_manager) = self.encryption_manager.take() {
            encryption_manager.leave();
        }
        self.own_participation = None;
        drop(upkeep);

        // Republish the joined memberships now that we are no longer among them, rather than
        // waiting for the host's next sticky delta. Room state and peer
        // candidates are deliberately kept: they are room truth a host feeding
        // deltas will never re-deliver, and a host that has hung up may still
        // want to see who is left in the call.
        self.refresh().await;

        sent?;
        log::info!("[{}] left", self.log_tag);
        Ok(())
    }

    /// One keep-alive tick: restarts the delayed leave, refreshes the sticky
    /// membership when due, and performs a due key rotation.
    ///
    /// The session's upkeep ticks this on an interval from join until leave;
    /// a host calls it only where the upkeep had no runtime to run on. If
    /// nothing ticks, the delayed leave fires and cleans up.
    ///
    /// Returns `false` if not joined (no membership machine active).
    pub async fn keep_alive(&mut self) -> bool {
        if let Some(machine) = self.own_membership_machine.as_ref() {
            upkeep::tick(machine, self.encryption_manager.as_ref(), &self.log_tag).await;
            true
        } else {
            log::debug!("[{}] keep-alive ignored: not joined", self.log_tag);
            false
        }
    }

    /// Returns the number of currently tracked joined members.
    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    /// Our `member.id` for the current join, or `None` while not joined.
    ///
    /// MSC4143 requires a fresh one on every join, so this changes across a
    /// leave/rejoin and must not be cached by the caller. Consumers that need
    /// it — the media layer derives its MSC4195 participant identity from it —
    /// should read it here rather than supply one, so it cannot drift from the
    /// value the membership was actually published under.
    pub fn own_member_id(&self) -> Option<&str> {
        self.own_membership_machine
            .as_ref()
            .map(|machine| machine.sticky_key())
    }

    /// The event id of our current membership event, or `None` while not
    /// joined.
    ///
    /// It moves on every sticky refresh, so read it at the moment of use.
    pub fn own_membership_event_id(&self) -> Option<String> {
        self.own_membership_machine
            .as_ref()
            .and_then(|machine| machine.membership_event_id())
    }

    /// Follows [`Self::own_membership_event_id`] for this join, which moves on
    /// every sticky refresh; `None` while not joined.
    pub fn subscribe_own_membership_event_id(&self) -> Option<watch::Receiver<Option<String>>> {
        self.own_membership_machine
            .as_ref()
            .map(|machine| machine.subscribe_membership_event_id())
    }

    /// Stops this join's upkeep when aborted, for an owner that lets go of
    /// the join without leaving; `None` while not joined or without one.
    pub fn upkeep_abort_handle(&self) -> Option<AbortHandle> {
        self.upkeep.as_ref().map(AbortOnDrop::abort_handle)
    }

    /// Whether a dead man's switch is protecting our membership.
    ///
    /// `false` once the homeserver has refused to arm an MSC4140 delayed leave —
    /// the call works, but a client that dies is cleaned up by the membership
    /// lifetime running out rather than promptly. `None` while not joined.
    pub fn delayed_leave_supported(&self) -> Option<bool> {
        self.own_membership_machine
            .as_ref()
            .map(|machine| machine.delayed_leave_supported())
    }

    /// Subscribes to full membership snapshots for this session as a watch receiver.
    ///
    /// This is used by bindings that implement their own polling/callback model.
    pub fn subscribe_membership_snapshots(&self) -> watch::Receiver<Vec<JoinedMembership>> {
        self.membership_snapshots_tx.subscribe()
    }

    /// Subscribes to the leaves this session makes on its own, without the
    /// host asking — today only [`LeaveCode::SlotClosed`]. A host that runs
    /// media ends it here; a leave the host asked for is not announced.
    pub fn subscribe_auto_leaves(&self) -> broadcast::Receiver<LeaveReason> {
        self.auto_leaves_tx.subscribe()
    }

    /// Applies the initial membership events for this single session.
    pub async fn set_current_state(
        &mut self,
        events: impl IntoIterator<Item = RtcMembershipEvent>,
    ) {
        // Replace, do not merge: `events` is the complete set for this slot, so
        // a candidate missing from it is gone.
        //
        // The distinction is not academic. A member does not only leave by
        // sending a leave event — an MSC4354 sticky entry expires when its owner
        // stops refreshing it, which is exactly what a crashed client does, and
        // an entry lapsing produces no event at all. Merging would leave that
        // member in the call for good, and force every host to diff snapshots
        // itself to notice.
        let previous = std::mem::take(&mut self.candidates);
        for event in events {
            // Deliberately *not* `apply_membership_event`: this loop rebuilds
            // the whole candidate set, and publishing after each event would
            // announce every partial set of joined memberships on the way — starting with a
            // one-member one, which reads as everybody else leaving. Refresh
            // happens once, below.
            self.record_membership_event(event);
        }

        let dropped: Vec<&str> = previous
            .iter()
            .filter(|before| {
                !self
                    .candidates
                    .iter()
                    .any(|now| now.sender == before.sender && now.sticky_key == before.sticky_key)
            })
            .map(|candidate| candidate.sticky_key.as_str())
            .collect();
        if !dropped.is_empty() {
            log::info!(
                "[{}] {} candidate(s) gone from the current sticky state (expired or \
                 withdrawn): {dropped:?}",
                self.log_tag,
                dropped.len(),
            );
        }

        // `apply_membership_event` refreshes only when it changed something, so
        // a purely shrinking update would otherwise never republish.
        self.refresh().await;
    }

    /// Applies one membership event to this session.
    pub async fn update(&mut self, event: RtcMembershipEvent) {
        self.apply_membership_event(event).await;
    }

    /// Applies one membership event and publishes the result.
    async fn apply_membership_event(&mut self, event: RtcMembershipEvent) {
        if self.record_membership_event(event) {
            self.refresh().await;
        }
    }

    /// Records one membership event against `candidates`, reporting whether it
    /// changed anything. **Publishes nothing.**
    ///
    /// Separate from [`apply_membership_event`](Self::apply_membership_event)
    /// because a batch must not publish per event. [`set_current_state`] rebuilds
    /// the candidate set from scratch, so refreshing inside the loop announces
    /// every intermediate state as though it were real: the first event of a
    /// six-member snapshot publishes a *one*-member set, which the encryption
    /// manager reads as five members leaving and answers with a key rotation.
    /// The five are re-added an instant later, so the joined memberships end up correct and
    /// the rotation is pure waste — once per sticky tick, per session, and with
    /// a to-device send to every remaining member. That is quadratic in the
    /// participant count and was rotating keys every few seconds in a ten-device
    /// call.
    ///
    /// [`set_current_state`]: Self::set_current_state
    fn record_membership_event(&mut self, event: RtcMembershipEvent) -> bool {
        match event {
            RtcMembershipEvent::Joined(joined) => {
                let existing = self.candidates.iter().position(|candidate| {
                    candidate.sender == joined.sender && candidate.sticky_key == joined.sticky_key
                });

                match existing {
                    Some(index) if self.candidates[index] == joined => {
                        log::trace!(
                            "[{}] member event unchanged, ignored: {}/{}",
                            self.log_tag,
                            joined.sender,
                            joined.sticky_key,
                        );
                        return false;
                    }
                    Some(index) => {
                        log::debug!(
                            "[{}] candidate updated: {}/{}",
                            self.log_tag,
                            joined.sender,
                            joined.sticky_key,
                        );
                        self.candidates[index] = joined;
                    }
                    None => {
                        log::debug!(
                            "[{}] candidate added: {}/{}",
                            self.log_tag,
                            joined.sender,
                            joined.sticky_key,
                        );
                        self.candidates.push(joined);
                    }
                }
            }
            RtcMembershipEvent::Left(left) => {
                let before = self.candidates.len();
                self.candidates.retain(|candidate| {
                    !(candidate.sender == left.sender && candidate.sticky_key == left.sticky_key)
                });

                if self.candidates.len() == before {
                    log::trace!(
                        "[{}] leave for an unknown candidate, ignored: {}/{}",
                        self.log_tag,
                        left.sender,
                        left.sticky_key,
                    );
                    return false;
                }

                log::debug!(
                    "[{}] candidate removed: {}/{}",
                    self.log_tag,
                    left.sender,
                    left.sticky_key,
                );
            }
        }

        true
    }

    /// Whether `candidate` satisfies the MSC4143 join conditions that depend on
    /// room state, and if not, which one it fails.
    ///
    /// The other two conditions are handled before this point: `membership =
    /// join` when the event is converted, and stickiness by the host's sticky
    /// map, whose removals arrive as leaves.
    fn join_condition(&self, candidate: &JoinedMembership) -> JoinCondition {
        match &self.slot {
            // Nothing has told us about the slot, so this condition cannot be
            // evaluated; enforcing it would drop every member.
            SlotKnowledge::Unsupplied => {}
            SlotKnowledge::Known(state) if state.is_open() => {}
            SlotKnowledge::Known(_) => return JoinCondition::SlotClosed,
        }

        // MSC4143: in an encrypted room `m.rtc.member` events MUST be
        // encrypted, and one that is not "MUST be considered left". An event
        // whose encryption the host did not report is not judged.
        if self.room_encryption == RoomEncryption::Encrypted
            && candidate.origin.was_encrypted() == Some(false)
        {
            return JoinCondition::UnencryptedInEncryptedRoom;
        }

        // A previous participation of this very device is us, one call ago —
        // MSC4143 mints a fresh `member.id` per join, and the old one stays
        // sticky until the homeserver expires it. Left in, it becomes a phantom
        // member: the media layer opens a receive stream for it and waits for a
        // key that will never come, and it forces a key rotation whose only
        // recipient is unreachable (a device cannot send itself a to-device
        // message). The encryption manager already filters it out of its
        // recipients; this is the same rule applied one level up, where the
        // published joined memberships are decided.
        //
        // The device must match, not just the user: other devices of our own
        // user are ordinary peers. A candidate whose sending device the host did
        // not report therefore cannot be judged, and stays.
        if let Some(own) = &self.own_participation
            && candidate.sender == own.user_id
            && candidate.origin.sender_device_id() == Some(own.device_id.as_str())
            && candidate.member_id != own.member_id
        {
            return JoinCondition::SupersededOwnParticipation;
        }

        match &self.room_members {
            Some(joined) if !joined.contains(&candidate.sender) => JoinCondition::SenderNotInRoom,
            _ => JoinCondition::Joined,
        }
    }

    /// Called only for the host's device-list updates. A new device does not
    /// need an already decryptable RTC membership to receive our fresh event,
    /// but its user must currently be joined to this encrypted open room.
    pub(crate) fn on_device_updates(&mut self, devices: &[(String, String)]) {
        if let (Some(machine), Some(own)) = (&self.own_membership_machine, &self.own_participation)
            && machine.state() == OwnMembershipState::Joined
            && self.peer_renewals.observe_devices(
                devices,
                own,
                &self.slot,
                self.room_members.as_ref(),
                self.room_encryption,
            )
        {
            machine.request_peer_renewal();
        }
    }

    /// Recomputes the joined set and publishes it if it changed.
    ///
    /// Every input to the join conditions routes through here, so a slot
    /// closing or a member leaving the room takes effect the same way a leave
    /// event does — including telling the encryption manager to stop sharing
    /// keys with whoever dropped out.
    async fn refresh(&mut self) {
        let mut members = Vec::with_capacity(self.candidates.len());
        let mut excluded: Vec<(&str, JoinCondition)> = Vec::new();

        for candidate in &self.candidates {
            let condition = self.join_condition(candidate);
            if condition.is_joined() {
                members.push(candidate.clone());
            } else {
                excluded.push((candidate.sticky_key.as_str(), condition));
            }
        }

        // Run even when the published vector is unchanged: newly supplied room
        // state can make an earlier candidate eligible for this optimization.
        // No network work happens here while the room/session is locked.
        if let (Some(machine), Some(own)) = (&self.own_membership_machine, &self.own_participation)
            && machine.state() == OwnMembershipState::Joined
        {
            match self.peer_renewals.observe(
                &members,
                own,
                &self.slot,
                self.room_members.as_ref(),
                self.room_encryption,
                false,
            ) {
                PeerRenewalAction::Request => machine.request_peer_renewal(),
                PeerRenewalAction::Disable => machine.disable_peer_renewal(),
                PeerRenewalAction::None => {}
            }
        }

        if members == self.members {
            if !excluded.is_empty() {
                log::debug!(
                    "[{}] membership unchanged, {} candidate(s) still excluded: {}",
                    self.log_tag,
                    excluded.len(),
                    describe_exclusions(&excluded),
                );
            }
            return;
        }

        let joined = sticky_keys(&members);
        let previous = sticky_keys(&self.members);
        let added = difference(&joined, &previous);
        let removed = difference(&previous, &joined);
        if added.is_empty() && removed.is_empty() {
            // Same members, different events: a peer refreshed its sticky
            // entry, or republished with new transports. Routine, so not
            // reported as a change of membership.
            log::debug!(
                "[{}] membership set unchanged ({} joined); member event(s) updated",
                self.log_tag,
                members.len(),
            );
        } else {
            log::info!(
                "[{}] membership changed: {} -> {} joined (+{:?} -{:?}) of {} candidate(s)",
                self.log_tag,
                self.members.len(),
                members.len(),
                added,
                removed,
                self.candidates.len(),
            );
        }
        if !excluded.is_empty() {
            log::debug!(
                "[{}] {} candidate(s) excluded: {}",
                self.log_tag,
                excluded.len(),
                describe_exclusions(&excluded),
            );
        }

        self.members = members;
        if let Some(encryption) = &self.encryption_manager {
            // The existing verified vector is borrowed before publishing its
            // watch snapshot; a lagging actor cannot roll admission backwards.
            encryption.on_membership_snapshot(&self.members);
        }
        self.membership_snapshots_tx
            .send_replace(self.members.clone());
        self.membership_scope.notify(&self.members);

        if let Some(ref encryption_manager) = self.encryption_manager {
            let _ = encryption_manager.on_memberships_update().await;
        }
    }

    /// Applies the room state governing this session's slot.
    ///
    /// Closing a slot leaves every member of it, and reopening it restores the
    /// ones whose member events are still sticky — MSC4143 requires clients to
    /// track the latest room state at all times, not just at join. Our own join
    /// is no exception: closing the slot under it sends our leave with
    /// [`LeaveCode::SlotClosed`] (see [`SlotSession::subscribe_auto_leaves`]).
    pub async fn set_slot_state(&mut self, state: SlotState) {
        if self.slot == SlotKnowledge::Known(state.clone()) {
            return;
        }
        log::info!(
            "[{}] slot state: {:?} -> {}",
            self.log_tag,
            self.slot,
            if state.is_open() { "Open" } else { "Closed" },
        );
        let closed = !state.is_open();
        self.slot = SlotKnowledge::Known(state);
        if closed && self.own_membership_machine.is_some() {
            self.auto_leave(LeaveReason::new(LeaveCode::SlotClosed))
                .await;
        } else {
            self.refresh().await;
        }
    }

    /// Leaves without the host asking, and tells it so — even when the send
    /// fails, since `leave` ends the join regardless.
    async fn auto_leave(&mut self, reason: LeaveReason) {
        log::info!("[{}] leaving on our own ({:?})", self.log_tag, reason.code);
        let params = LeaveSessionParams {
            leave_reason: Some(reason.clone()),
        };
        if let Err(error) = self.leave(params).await {
            log::warn!(
                "[{}] could not send our leave; the delayed leave will: {error}",
                self.log_tag
            );
        }
        let _ = self.auto_leaves_tx.send(reason);
    }

    /// Returns this session's slot to [`SlotKnowledge::Unsupplied`], so the
    /// open-slot condition stops being enforced.
    ///
    /// Not the same as being told the slot is open: "unknowable" and "open" agree
    /// on today's outcome and disagree about what a later `m.rtc.slot` means. The
    /// caller is saying it can no longer speak for this room's slots, not that it
    /// has seen one.
    pub(crate) async fn forget_slot_state(&mut self) {
        if self.slot == SlotKnowledge::Unsupplied {
            return;
        }
        log::info!(
            "[{}] slot state: {:?} -> Unsupplied (no longer enforced)",
            self.log_tag,
            self.slot,
        );
        self.slot = SlotKnowledge::Unsupplied;
        self.refresh().await;
    }

    /// Sets the slot state on a session that has no members yet.
    ///
    /// Used when a session is created after its room state was already known;
    /// [`SlotSession::set_slot_state`] is the one to use once it is live.
    pub(crate) fn seed_slot_state(&mut self, state: SlotState) {
        debug_assert!(self.candidates.is_empty(), "seeding a populated session");
        self.slot = SlotKnowledge::Known(state);
    }

    /// Sets the room members on a session that has no members yet.
    pub(crate) fn seed_room_members(&mut self, room_members: HashSet<String>) {
        debug_assert!(self.candidates.is_empty(), "seeding a populated session");
        self.room_members = Some(room_members);
    }

    /// Sets the room encryption state on a session that has no members yet.
    pub(crate) fn seed_room_encryption(&mut self, room_encryption: RoomEncryption) {
        debug_assert!(self.candidates.is_empty(), "seeding a populated session");
        self.room_encryption = room_encryption;
    }

    /// Sets whether the room is encrypted.
    ///
    /// In an encrypted room, members whose `m.rtc.member` event arrived in the
    /// clear stop counting as joined.
    pub async fn set_room_encryption(&mut self, room_encryption: RoomEncryption) {
        if self.room_encryption == room_encryption {
            return;
        }
        log::info!(
            "[{}] room encryption: {:?} -> {room_encryption:?}",
            self.log_tag,
            self.room_encryption,
        );
        self.room_encryption = room_encryption;
        self.refresh().await;
    }

    /// Whether RTC data should be encrypted, per the slot and the room.
    ///
    /// `None` means there is nothing to negotiate from — no room state has been
    /// supplied — and the caller's own configuration stands. Otherwise this is
    /// authoritative: MSC4143 prescribes the mechanism through the slot's
    /// `encryption` object, and forbids encryption outright in unencrypted
    /// rooms, so a local preference cannot override either way.
    ///
    /// NOTE: this is read when the session joins. A slot whose mechanism changes
    /// mid-session is not renegotiated; the dangerous direction (the slot
    /// closing) is already covered, since that leaves every member and so stops
    /// key distribution.
    pub fn negotiated_encryption(&self) -> Option<bool> {
        let SlotKnowledge::Known(state) = &self.slot else {
            return None;
        };

        Some(
            state
                .open()
                .and_then(|slot| slot.mechanism.as_ref())
                .is_some_and(|mechanism| mechanism.is_supported()),
        )
    }

    /// Everything this session believes, as JSON, for bug reports.
    ///
    /// Logs tell you what happened; this tells you where things ended up, which
    /// is the other half of diagnosing "the joined memberships are wrong". Deliberately
    /// includes the *candidates* and why each is excluded — the joined set
    /// alone cannot explain an absence.
    pub fn debug_snapshot(&self) -> serde_json::Value {
        let candidates: Vec<serde_json::Value> = self
            .candidates
            .iter()
            .map(|candidate| {
                serde_json::json!({
                    "sender": candidate.sender,
                    "sticky_key": candidate.sticky_key,
                    "member_id": candidate.member_id,
                    "was_encrypted": candidate.origin.was_encrypted(),
                    "condition": format!("{:?}", self.join_condition(candidate)),
                })
            })
            .collect();

        serde_json::json!({
            "slot": match &self.slot {
                SlotKnowledge::Unsupplied => "Unsupplied".to_owned(),
                SlotKnowledge::Known(state) if state.is_open() => "Open".to_owned(),
                SlotKnowledge::Known(_) => "Closed".to_owned(),
            },
            "room_encryption": format!("{:?}", self.room_encryption),
            "room_members_known": self.room_members.as_ref().map(HashSet::len),
            "negotiated_encryption": self.negotiated_encryption(),
            "joined_count": self.members.len(),
            "joined": sticky_keys(&self.members),
            "candidates": candidates,
            "has_backend": self.backend.is_some(),
            "own_membership": self
                .own_membership_machine
                .as_ref()
                .map(|machine| machine.sticky_key()),
            "own_membership_event_id": self.own_membership_event_id(),
            "delayed_leave_supported": self
                .own_membership_machine
                .as_ref()
                .map(|machine| machine.delayed_leave_supported()),
            "has_encryption_manager": self.encryption_manager.is_some(),
        })
    }

    /// The slot state this session is applying, if any has been supplied.
    pub fn slot_state(&self) -> Option<&SlotState> {
        match &self.slot {
            SlotKnowledge::Known(state) => Some(state),
            SlotKnowledge::Unsupplied => None,
        }
    }

    /// Sets the users currently joined to the room.
    ///
    /// Until this is called the room-membership condition is not enforced. A
    /// member who leaves the room stops being joined to the slot even if their
    /// member event is still sticky.
    pub async fn set_room_members(&mut self, room_members: HashSet<String>) {
        if self.room_members.as_ref() == Some(&room_members) {
            return;
        }
        log::debug!(
            "[{}] room members: {:?} -> {} joined",
            self.log_tag,
            self.room_members.as_ref().map(HashSet::len),
            room_members.len(),
        );
        self.room_members = Some(room_members);
        self.refresh().await;
    }
}

/// MSC4143: the intended membership status carried in `content.member.membership`.
///
/// Unknown values round-trip through [`Membership::Unknown`] rather than failing
/// the whole event: a member event using a status from a future spec revision
/// still has to parse, and simply doesn't count as joined.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Membership {
    /// The member intends to be joined to the slot.
    Join,
    /// The member has left the slot.
    Leave,
    /// A status this client does not understand; treated as left.
    #[serde(untagged)]
    Unknown(String),
}

/// MSC4143: Member object (`content.member`).
///
/// Note that the pre-2026 `claimed_user_id` / `claimed_device_id` fields are gone:
/// the sending user is authenticated by the event's `sender`, and the sending
/// device by the event's decryption metadata (see
/// [`JoinedMembership::sender_device_id`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    /// Identifier distinguishing this participation, unique per join.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The intended membership status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub membership: Option<Membership>,
}

impl MemberInfo {
    /// True when no member fields are set (used to skip serialization).
    pub fn is_empty(&self) -> bool {
        self.id.is_none() && self.membership.is_none()
    }

    /// True when this member object declares `membership = join` with a usable id.
    pub fn is_join(&self) -> bool {
        matches!(self.membership, Some(Membership::Join))
            && self.id.as_deref().is_some_and(|id| !id.is_empty())
    }
}

/// MSC4143 `content.application`. The core reads only `type`; the rest is the
/// application's and passes through untouched, in both directions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicationInfo {
    #[serde(rename = "type")]
    pub application_type: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl ApplicationInfo {
    /// An application of `application_type` with no further properties.
    pub fn new(application_type: impl Into<String>) -> Self {
        Self {
            application_type: Some(application_type.into()),
            extra: BTreeMap::new(),
        }
    }

    /// Adds an application-defined property, e.g. `m.call.voice_only`.
    pub fn with_extra(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }

    /// The application `type`, when one is set and non-empty.
    pub fn application_type(&self) -> Option<&str> {
        self.application_type.as_deref().filter(|t| !t.is_empty())
    }

    /// True when no application fields are set (used to skip serialization for leave events).
    pub fn is_empty(&self) -> bool {
        self.application_type.is_none() && self.extra.is_empty()
    }
}

impl From<String> for ApplicationInfo {
    fn from(application_type: String) -> Self {
        Self::new(application_type)
    }
}

impl From<&str> for ApplicationInfo {
    fn from(application_type: &str) -> Self {
        Self::new(application_type)
    }
}

/// MSC4143: the generic `leave_reason.code` values defined by the core proposal.
///
/// Applications and transports may define further codes, which land in
/// [`LeaveCode::Other`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaveCode {
    /// The member left intentionally (e.g. by hanging up a call).
    Leave,
    /// The member left through a scheduled delayed leave event.
    DelayedLeave,
    /// The member left because the slot was closed mid-session.
    SlotClosed,
    /// A code defined outside this proposal.
    #[serde(untagged)]
    Other(String),
}

impl LeaveCode {
    /// Parses a wire `leave_reason.code`, keeping application- and
    /// transport-defined codes intact as [`LeaveCode::Other`].
    ///
    /// The bindings use this so every entry point agrees on the mapping.
    pub fn from_code(code: &str) -> Self {
        match code {
            "leave" => Self::Leave,
            "delayed_leave" => Self::DelayedLeave,
            "slot_closed" => Self::SlotClosed,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The wire `leave_reason.code`; the inverse of [`LeaveCode::from_code`].
    pub fn as_str(&self) -> &str {
        match self {
            Self::Leave => "leave",
            Self::DelayedLeave => "delayed_leave",
            Self::SlotClosed => "slot_closed",
            Self::Other(code) => code,
        }
    }
}

/// MSC4143: Leave reason (`content.leave_reason`).
///
/// Replaces the earlier `disconnect_reason` object. `code` is the machine-readable
/// identifier and `reason` the optional human-readable explanation — note that in
/// the old shape `reason` was the machine-readable half.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaveReason {
    /// Identifier for the specific leave cause.
    pub code: LeaveCode,
    /// Optional human-readable explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl LeaveReason {
    /// A leave reason carrying just a code.
    pub fn new(code: LeaveCode) -> Self {
        Self { code, reason: None }
    }

    /// A leave reason with a human-readable explanation.
    pub fn with_reason(code: LeaveCode, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: Some(reason.into()),
        }
    }
}

#[derive(Clone, Debug)]
/// Membership event projection derived from sticky event content.
pub enum RtcMembershipEvent {
    /// A member is connected for the slot.
    Joined(JoinedMembership),
    /// A member is disconnected for the slot.
    Left(LeftMembership),
}

/// MSC4143: Joined membership payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinedMembership {
    /// Room where the membership is active.
    pub room_id: String,
    /// MatrixRTC slot identifier.
    pub slot_id: String,
    /// Sender user ID of the membership event.
    pub sender: String,
    /// How the member event reached us.
    ///
    /// MSC4143 removed the self-asserted `member.claimed_device_id`, so the
    /// sending device comes from here — key distribution targets "the devices
    /// that were used to encrypt these member events".
    pub origin: EventOrigin,
    /// Sticky key identifying this membership stream (equal to `member_id`).
    pub sticky_key: String,
    /// `member.id` — identifies this participation, unique per join.
    pub member_id: String,
    /// Event id of the *latest* member event of this participation, when the
    /// host reported one.
    ///
    /// Latest, not first: a sticky refresh re-sends the membership and the new
    /// event replaces the old one in the sticky map, so this moves on every
    /// refresh — exactly as matrix-js-sdk's `CallMembership.eventId` does.
    pub membership_event_id: Option<String>,
    /// When this participation began (ms since the epoch), if the dialect
    /// states it.
    ///
    /// `None` for a native MSC4143 membership, whose `member_id` is already
    /// fresh per join. The pre-sticky Element Call dialect reuses
    /// `{user}:{device}` as the member id across joins, and there this is the
    /// event's `created_ts` — pinned for the life of one join, new on the
    /// next — so `(member_id, membership_ts)` distinguishes two joins by the
    /// same device where `member_id` alone cannot. The js-sdk's encryption
    /// manager keys on the same value as `membershipTs`.
    pub membership_ts: Option<u64>,
    /// `origin_server_ts` of the *latest* member event, when the host reported
    /// one. Moves on every sticky refresh, like `membership_event_id`; a
    /// consumer wanting a join time pins the first value it sees. Never an
    /// identity: keys stay on `(member_id, membership_ts)`.
    pub origin_server_ts: Option<u64>,
    /// `content.application`; a joined membership always has a `type`.
    pub application: ApplicationInfo,
    /// Transports this member publishes on (`content.transports.published`).
    pub transports: Vec<RtcTransport>,
    /// Transport types this member can subscribe to
    /// (`content.transports.can_subscribe`).
    pub can_subscribe: Vec<String>,
}

/// MSC4143: Left membership payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeftMembership {
    /// Room where the membership was active.
    pub room_id: String,
    /// MatrixRTC slot identifier.
    pub slot_id: String,
    /// Sender user ID of the membership event.
    pub sender: String,
    /// Sticky key identifying this membership stream.
    pub sticky_key: String,
    /// `member.id`, when the event carried one. Absent on malformed events and
    /// on sticky removals, which are treated as leaves regardless of content.
    pub member_id: Option<String>,
    /// Optional leave reason (MSC4143).
    pub leave_reason: Option<LeaveReason>,
}

#[cfg(test)]
mod peer_renewal_tests {
    use super::*;
    use crate::host::backend::MockBackend;
    use crate::membership_listener::MembershipListeners;
    use crate::slot::{EncryptionMechanism, OpenSlot, SlotEncryption};

    fn own() -> OwnParticipation {
        OwnParticipation {
            user_id: "@mock:example.org".into(),
            device_id: "MOCKDEVICE".into(),
            member_id: "own-generation".into(),
        }
    }

    fn peer(id: &str) -> JoinedMembership {
        JoinedMembership {
            room_id: "!room:example.org".into(),
            slot_id: "m.call#room".into(),
            sender: "@peer:example.org".into(),
            origin: EventOrigin::Encrypted {
                sender_device_id: Some("PEER".into()),
            },
            sticky_key: id.into(),
            member_id: id.into(),
            membership_event_id: Some("$peer".into()),
            membership_ts: None,
            origin_server_ts: Some(1),
            application: ApplicationInfo::new("m.call"),
            transports: vec![],
            can_subscribe: vec![],
        }
    }

    fn open_slot() -> SlotState {
        SlotState::Open(OpenSlot {
            application_type: "m.call".into(),
            application_extra: BTreeMap::new(),
            encryption: Some(SlotEncryption {
                encryption_type: "m.per_member".into(),
                extra: BTreeMap::new(),
            }),
            mechanism: Some(EncryptionMechanism::PerMember),
        })
    }

    fn room_users() -> HashSet<String> {
        ["@mock:example.org", "@peer:example.org"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    fn ledger() -> PeerRenewalLedger {
        PeerRenewalLedger {
            active: true,
            ..PeerRenewalLedger::default()
        }
    }

    fn observe(ledger: &mut PeerRenewalLedger, members: &[JoinedMembership]) -> PeerRenewalAction {
        ledger.observe(
            members,
            &own(),
            &SlotKnowledge::Known(open_slot()),
            Some(&room_users()),
            RoomEncryption::Encrypted,
            false,
        )
    }

    #[test]
    fn peer_renewal_rejects_unattributed_own_or_invalid_memberships() {
        let peer = peer("peer-generation");
        let mut rejected = vec![];
        for origin in [
            EventOrigin::Unknown,
            EventOrigin::Cleartext,
            EventOrigin::Claimed {
                device_id: "PEER".into(),
            },
            EventOrigin::Encrypted {
                sender_device_id: None,
            },
            EventOrigin::Encrypted {
                sender_device_id: Some(String::new()),
            },
        ] {
            let mut item = peer.clone();
            item.origin = origin;
            rejected.push(item);
        }
        let mut same_device = peer.clone();
        same_device.sender = own().user_id;
        same_device.origin = EventOrigin::Encrypted {
            sender_device_id: Some(own().device_id),
        };
        rejected.push(same_device); // Also rejects superseded local generations.
        let mut non_member = peer.clone();
        non_member.sender = "@absent:example.org".into();
        rejected.push(non_member);
        let mut mismatched = peer.clone();
        mismatched.sticky_key = "different".into();
        rejected.push(mismatched);
        for bad in [
            String::new(),
            "a".repeat(PEER_RENEWAL_ID_BYTES + 1),
            "bad\nmember".into(),
        ] {
            let mut item = peer.clone();
            item.member_id = bad.clone();
            item.sticky_key = bad;
            rejected.push(item);
        }
        for item in rejected {
            let mut seen = ledger();
            assert_eq!(observe(&mut seen, &[item]), PeerRenewalAction::None);
            assert!(seen.seen.is_empty());
        }
        let mut another_own_device = peer.clone();
        another_own_device.sender = own().user_id;
        assert_eq!(
            observe(&mut ledger(), &[another_own_device]),
            PeerRenewalAction::Request
        );
    }

    #[test]
    fn peer_renewal_requires_known_encrypted_room_open_slot_and_room_members() {
        let peers = [peer("generation")];
        for encryption in [RoomEncryption::Unknown, RoomEncryption::Unencrypted] {
            assert_eq!(
                ledger().observe(
                    &peers,
                    &own(),
                    &SlotKnowledge::Known(open_slot()),
                    Some(&room_users()),
                    encryption,
                    false
                ),
                PeerRenewalAction::None
            );
        }
        for slot in [
            SlotKnowledge::Unsupplied,
            SlotKnowledge::Known(SlotState::Closed),
        ] {
            assert_eq!(
                ledger().observe(
                    &peers,
                    &own(),
                    &slot,
                    Some(&room_users()),
                    RoomEncryption::Encrypted,
                    false
                ),
                PeerRenewalAction::None
            );
        }
        assert_eq!(
            ledger().observe(
                &peers,
                &own(),
                &SlotKnowledge::Known(open_slot()),
                None,
                RoomEncryption::Encrypted,
                false
            ),
            PeerRenewalAction::None
        );
        assert_eq!(
            observe(&mut PeerRenewalLedger::default(), &peers),
            PeerRenewalAction::None
        );
    }

    #[test]
    fn peer_renewal_tombstones_refresh_reorder_disappearance_and_replay() {
        let mut seen = ledger();
        let first = peer("generation-one");
        let second = peer("generation-two");
        assert_eq!(
            observe(&mut seen, &[first.clone(), second.clone()]),
            PeerRenewalAction::Request
        );
        let mut refreshed = first.clone();
        refreshed.membership_event_id = Some("$new-event".into());
        refreshed.origin_server_ts = Some(999);
        refreshed.can_subscribe.push("livekit".into());
        assert_eq!(
            observe(&mut seen, &[second.clone(), refreshed.clone(), refreshed]),
            PeerRenewalAction::None
        );
        assert_eq!(observe(&mut seen, &[]), PeerRenewalAction::None);
        assert_eq!(
            observe(&mut seen, &[first, second]),
            PeerRenewalAction::None
        );
        assert_eq!(
            observe(&mut seen, &[peer("real-rejoin-generation")]),
            PeerRenewalAction::Request
        );
        assert_eq!(seen.seen.len(), 3);
    }

    #[test]
    fn peer_renewal_baseline_and_capacity_never_evict_replay_history() {
        let mut seen = ledger();
        assert_eq!(
            seen.observe(
                &[peer("baseline")],
                &own(),
                &SlotKnowledge::Known(open_slot()),
                Some(&room_users()),
                RoomEncryption::Encrypted,
                true
            ),
            PeerRenewalAction::None
        );
        assert_eq!(
            observe(&mut seen, &[peer("baseline")]),
            PeerRenewalAction::None
        );
        let peers: Vec<_> = (1..PEER_RENEWAL_TOMBSTONES)
            .map(|i| peer(&format!("generation-{i}")))
            .collect();
        assert_eq!(observe(&mut seen, &peers), PeerRenewalAction::Request);
        assert_eq!(seen.seen.len(), PEER_RENEWAL_TOMBSTONES);
        assert_eq!(
            observe(&mut seen, &[peer("overflow")]),
            PeerRenewalAction::Disable
        );
        assert!(!seen.active);
        assert_eq!(seen.seen.len(), PEER_RENEWAL_TOMBSTONES);
        assert_eq!(observe(&mut seen, &[]), PeerRenewalAction::None);
        assert_eq!(
            observe(&mut seen, &[peer("overflow"), peer("another")]),
            PeerRenewalAction::None
        );
    }

    fn session(backend: Arc<MockBackend>) -> SlotSession<MockBackend> {
        let mut session = SlotSession::new(
            "!room:example.org".into(),
            "m.call#room".into(),
            Some(backend),
            MembershipScope {
                slot_id: "m.call#room".into(),
                listeners: MembershipListeners::default(),
            },
        );
        session.seed_slot_state(open_slot());
        session.seed_room_members(room_users());
        session.seed_room_encryption(RoomEncryption::Encrypted);
        session
    }

    async fn join(session: &mut SlotSession<MockBackend>, id: &str) {
        session
            .join(
                JoinSessionParams::application("m.call")
                    .member_id(id)
                    .transport(TransportIntent::ReceiveOnly {
                        can_subscribe: vec!["livekit".into()],
                    }),
            )
            .await
            .unwrap();
        // Drive the production heartbeat deterministically in this test.
        drop(session.upkeep.take());
    }

    #[test]
    fn device_renewal_requires_room_authority_and_never_evicts_tombstones() {
        let devices = vec![("@peer:example.org".into(), "NEWDEVICE".into())];
        let mut seen = ledger();
        let own = own();
        let slot = SlotKnowledge::Known(open_slot());
        let users = room_users();
        assert!(!seen.observe_devices(&devices, &own, &slot, None, RoomEncryption::Encrypted));
        assert!(!seen.observe_devices(
            &devices,
            &own,
            &slot,
            Some(&users),
            RoomEncryption::Unknown,
        ));
        assert!(!seen.observe_devices(
            &devices,
            &own,
            &SlotKnowledge::Unsupplied,
            Some(&users),
            RoomEncryption::Encrypted,
        ));
        let ignored = vec![
            (own.user_id.clone(), own.device_id.clone()),
            ("@outside:example.org".into(), "NEWDEVICE".into()),
            ("@peer:example.org".into(), "bad\n".into()),
            (
                "@peer:example.org".into(),
                "x".repeat(PEER_RENEWAL_ID_BYTES + 1),
            ),
        ];
        assert!(!seen.observe_devices(
            &ignored,
            &own,
            &slot,
            Some(&users),
            RoomEncryption::Encrypted,
        ));
        let full = (0..PEER_RENEWAL_TOMBSTONES)
            .map(|index| ("@peer:example.org".into(), format!("DEVICE{index}")))
            .collect::<Vec<_>>();
        assert!(seen.observe_devices(&full, &own, &slot, Some(&users), RoomEncryption::Encrypted));
        assert!(!seen.observe_devices(&full, &own, &slot, Some(&users), RoomEncryption::Encrypted));
        assert!(!seen.observe_devices(
            &devices,
            &own,
            &slot,
            Some(&users),
            RoomEncryption::Encrypted,
        ));
        assert_eq!(seen.seen_devices.len(), PEER_RENEWAL_TOMBSTONES);
        assert!(seen.active);
        // Exhausting device hints does not disable normal membership renewal.
        assert_eq!(
            observe(&mut seen, &[peer("ordinary-peer")]),
            PeerRenewalAction::Request
        );
    }

    #[tokio::test]
    async fn handler_install_sees_borrowed_current_members_before_watch_updates() {
        struct Handler {
            watched: watch::Receiver<Vec<JoinedMembership>>,
            seen: Arc<std::sync::Mutex<Vec<(usize, Vec<String>, Vec<String>)>>>,
        }
        #[async_trait::async_trait]
        impl EncryptionKeySignalHandler for Handler {
            async fn on_new_key_material(&self, _signal: crate::KeyMaterialSignal) {}

            fn on_memberships_updated(&self, members: &[JoinedMembership]) {
                let watched = self.watched.borrow();
                self.seen.lock().unwrap().push((
                    members.as_ptr() as usize,
                    members
                        .iter()
                        .map(|member| member.member_id.clone())
                        .collect(),
                    watched
                        .iter()
                        .map(|member| member.member_id.clone())
                        .collect(),
                ));
            }
        }
        let backend = Arc::new(MockBackend::new());
        let mut session = session(backend);
        join(&mut session, "own-generation").await;
        session
            .set_current_state([RtcMembershipEvent::Joined(peer("before"))])
            .await;
        let watched = session.subscribe_membership_snapshots();
        let early_view = watched.borrow().clone();
        session
            .set_current_state([RtcMembershipEvent::Joined(peer("current"))])
            .await;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        assert!(session.set_encryption_signal_handler(Arc::new(Handler {
            watched,
            seen: Arc::clone(&seen),
        })));
        let initial = seen.lock().unwrap().clone();
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].0, session.members.as_ptr() as usize);
        assert_eq!(initial[0].1, vec!["current"]);
        assert_eq!(early_view[0].member_id, "before");
        session
            .set_current_state([RtcMembershipEvent::Joined(peer("next"))])
            .await;
        let observed = seen.lock().unwrap();
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[1].1, vec!["next"]);
        assert_eq!(observed[1].2, vec!["current"]);
    }

    #[tokio::test]
    async fn device_discovery_renews_only_current_own_membership_without_peer_event() {
        let backend = Arc::new(MockBackend::new());
        let mut session = session(backend.clone());
        let devices = vec![("@peer:example.org".into(), "NEWDEVICE".into())];
        session.on_device_updates(&devices);
        assert!(backend.sticky_events.lock().unwrap().is_empty());
        join(&mut session, "own-generation").await;
        let original = backend.sticky_events.lock().unwrap()[0].clone();
        assert!(session.members.is_empty());
        session.on_device_updates(&devices);
        session.keep_alive().await;
        {
            let events = backend.sticky_events.lock().unwrap();
            assert_eq!(events.len(), 2);
            assert_eq!(events[1], original);
        }
        assert!(session.members.is_empty());
        assert_eq!(session.peer_renewals.seen_devices.len(), 1);
        session.on_device_updates(&devices);
        session.keep_alive().await;
        assert_eq!(backend.sticky_events.lock().unwrap().len(), 2);
        session.leave(LeaveSessionParams::default()).await.unwrap();
        session.on_device_updates(&devices);
        assert_eq!(backend.sticky_events.lock().unwrap().len(), 3);
        assert!(session.peer_renewals.seen_devices.is_empty());
        join(&mut session, "new-own-generation").await;
        session.on_device_updates(&devices);
        session.keep_alive().await;
        assert_eq!(backend.sticky_events.lock().unwrap().len(), 5);
        session.leave(LeaveSessionParams::default()).await.unwrap();
    }

    #[tokio::test]
    async fn peer_renewal_real_session_refresh_sends_once_and_resets_on_own_join() {
        let backend = Arc::new(MockBackend::new());
        let mut session = session(backend.clone());
        let baseline = peer("baseline");
        session
            .set_current_state([RtcMembershipEvent::Joined(baseline.clone())])
            .await;
        join(&mut session, "own-generation").await;
        session.keep_alive().await;
        assert_eq!(backend.sticky_events.lock().unwrap().len(), 1);
        let arrived = peer("new-device-generation");
        session
            .set_current_state([
                RtcMembershipEvent::Joined(baseline.clone()),
                RtcMembershipEvent::Joined(arrived.clone()),
            ])
            .await;
        session.keep_alive().await;
        assert_eq!(backend.sticky_events.lock().unwrap().len(), 2);
        assert_eq!(
            session
                .own_membership_machine
                .as_ref()
                .unwrap()
                .membership_event_id()
                .as_deref(),
            Some("$sticky-2")
        );
        session.set_current_state([]).await;
        session
            .set_current_state([RtcMembershipEvent::Joined(arrived.clone())])
            .await;
        session.keep_alive().await;
        assert_eq!(backend.sticky_events.lock().unwrap().len(), 2);
        let cloned = session.clone();
        assert!(!cloned.peer_renewals.active);
        assert!(cloned.peer_renewals.seen.is_empty());
        assert!(cloned.own_membership_machine.is_none());
        session.leave(LeaveSessionParams::default()).await.unwrap();
        assert!(!session.peer_renewals.active);
        assert!(session.peer_renewals.seen.is_empty());
        join(&mut session, "own-new-generation").await;
        session.keep_alive().await;
        assert_eq!(backend.sticky_events.lock().unwrap().len(), 4); // join, refresh, leave, new join
        assert!(
            session
                .peer_renewals
                .seen
                .iter()
                .any(|identity| identity.2 == arrived.member_id)
        );
        session.leave(LeaveSessionParams::default()).await.unwrap();
    }

    #[tokio::test]
    async fn peer_renewal_later_room_evidence_triggers_even_if_roster_is_unchanged() {
        let backend = Arc::new(MockBackend::new());
        let mut session = session(backend.clone());
        session.room_members = None;
        join(&mut session, "own-generation").await;
        session
            .set_current_state([RtcMembershipEvent::Joined(peer("late-attributed"))])
            .await;
        session.keep_alive().await;
        assert_eq!(backend.sticky_events.lock().unwrap().len(), 1);
        // The ordinary roster was already permissive when room membership was
        // unknown; supplying known room state leaves that roster unchanged.
        session.set_room_members(room_users()).await;
        session.keep_alive().await;
        assert_eq!(backend.sticky_events.lock().unwrap().len(), 2);
        session.leave(LeaveSessionParams::default()).await.unwrap();
    }
    #[tokio::test]
    async fn peer_renewal_capacity_cancels_pending_without_stopping_ordinary_membership() {
        let backend = Arc::new(MockBackend::new());
        let mut session = session(backend.clone());
        join(&mut session, "own-generation").await;
        session
            .set_current_state([RtcMembershipEvent::Joined(peer("first"))])
            .await;
        // Before the heartbeat handles the pending request, the replay ledger
        // fills. No eviction and no optional send survive exhaustion.
        let many = (0..PEER_RENEWAL_TOMBSTONES)
            .map(|i| RtcMembershipEvent::Joined(peer(&format!("more-{i}"))));
        session.set_current_state(many).await;
        assert!(!session.peer_renewals.active);
        assert_eq!(session.peer_renewals.seen.len(), PEER_RENEWAL_TOMBSTONES);
        session.keep_alive().await;
        assert_eq!(backend.sticky_events.lock().unwrap().len(), 1);
        assert!(!backend.restarted_events.lock().unwrap().is_empty());
        session.leave(LeaveSessionParams::default()).await.unwrap();
        assert_eq!(backend.sticky_events.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn peer_renewal_new_attribution_can_make_an_existing_candidate_eligible() {
        let backend = Arc::new(MockBackend::new());
        let mut session = session(backend.clone());
        join(&mut session, "own-generation").await;
        let mut unknown = peer("generation");
        unknown.origin = EventOrigin::Unknown;
        session
            .set_current_state([RtcMembershipEvent::Joined(unknown)])
            .await;
        session.keep_alive().await;
        assert_eq!(backend.sticky_events.lock().unwrap().len(), 1);
        session
            .set_current_state([RtcMembershipEvent::Joined(peer("generation"))])
            .await;
        session.keep_alive().await;
        assert_eq!(backend.sticky_events.lock().unwrap().len(), 2);
        session.leave(LeaveSessionParams::default()).await.unwrap();
    }

    #[tokio::test]
    async fn peer_renewal_does_not_add_a_task_or_owner_beyond_session_upkeep() {
        let backend = Arc::new(MockBackend::new());
        let mut session = session(backend.clone());
        session
            .join(
                JoinSessionParams::application("m.call")
                    .member_id("own-generation")
                    .transport(TransportIntent::ReceiveOnly {
                        can_subscribe: vec!["livekit".into()],
                    }),
            )
            .await
            .unwrap();
        let weak = Arc::downgrade(session.own_membership_machine.as_ref().unwrap());
        session
            .set_current_state([RtcMembershipEvent::Joined(peer("generation"))])
            .await;
        drop(session);
        tokio::time::timeout(Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(backend.sticky_events.lock().unwrap().len(), 1);
    }
}
