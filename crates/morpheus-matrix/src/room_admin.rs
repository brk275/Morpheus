//! Revocable, exact-room member administration. The roster comes from the
//! account's synced member store; power levels and every write precondition
//! are fresh server reads. Each write is sent once, never retried, and ends
//! with an authoritative read of the affected state. The homeserver still
//! makes the final authorization decision; local checks only refuse requests
//! that the current room state already rules out.

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    future::Future,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use matrix_sdk::{
    HttpError, Room, RoomMemberships,
    config::RequestConfig,
    deserialized_responses::MemberEvent,
    room::RoomMember,
    ruma::{
        Int, OwnedEventId, OwnedUserId, RoomId, UserId,
        api::{
            client::{
                membership::{
                    ban_user,
                    invite_user::{
                        self,
                        v3::{InvitationRecipient, InviteUserId},
                    },
                    kick_user, unban_user,
                },
                state::send_state_event,
            },
            error::ErrorKind,
        },
        events::{
            AnyStateEventContent, EmptyStateKey, StateEventType,
            room::{
                create::RoomCreateEventContent,
                encryption::RoomEncryptionEventContent,
                guest_access::{GuestAccess, RoomGuestAccessEventContent},
                history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
                member::{MembershipState, RoomMemberEventContent},
                power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent, UserPowerLevel},
            },
        },
        serde::Raw,
    },
    sync::{RoomUpdate, State as SyncState},
};
use serde_json::Value;
use tokio::{
    sync::{Mutex, broadcast},
    time,
};

use crate::{
    AccountManager, AccountSelection, RoomSettingsCancellation, account_room,
    room_settings::{self, READ_TIMEOUT, REQUEST_TIMEOUT, StateEnvelope, WRITE_TIMEOUT},
};

pub const MAX_ADMIN_REASON_BYTES: usize = 512;
/// Roster rows retained by one session. Larger rooms report `truncated`.
pub const MAX_ROSTER_MEMBERS: usize = 20_000;
const MAX_USER_ID_BYTES: usize = 255;
const MAX_DISPLAY_NAME_BYTES: usize = 256;
const MAX_POWER_LEVELS_BYTES: usize = 60 * 1024;
// A verified write is shown until sync stores the same member event, or for
// at most this long; the synced store is authoritative again afterwards.
const OVERRIDE_TTL: Duration = Duration::from_secs(90);
const MAX_OVERRIDES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RoomAdminError {
    #[error("This member list is closed")]
    Cancelled,
    #[error("The selected account changed")]
    AccountChanged,
    #[error("Join this room before managing its members")]
    NotJoined,
    #[error("Room members are unavailable; refresh before retrying")]
    Unavailable,
    #[error("The homeserver is limiting requests; wait and refresh before retrying")]
    RateLimited,
    #[error("The operation timed out; refresh before retrying")]
    TimedOut,
    #[error("The server returned unsupported or invalid room state")]
    InvalidState,
    #[error("The homeserver refused this change with your current permissions")]
    Forbidden,
    #[error("Your current power level does not allow this change")]
    InsufficientPower,
    #[error("Room creators cannot be removed and their level cannot change")]
    CreatorProtected,
    #[error("Leave the room instead of removing yourself")]
    SelfTarget,
    #[error("This member changed on the server; review the refreshed roster")]
    TargetChanged,
    #[error("This user is already a member of the room")]
    AlreadyJoined,
    #[error("This user already has a pending invitation")]
    AlreadyInvited,
    #[error("This user is banned; unban them before inviting")]
    TargetBanned,
    #[error("This action does not apply to the member's current state")]
    NotApplicable,
    #[error("Enter a full Matrix user ID")]
    InvalidUserId,
    #[error("Enter a whole-number power level in Matrix's range")]
    InvalidPowerLevel,
    #[error("Use a reason of at most 512 bytes")]
    InvalidReason,
}
type Result<T> = std::result::Result<T, RoomAdminError>;

impl From<room_settings::RoomSettingsError> for RoomAdminError {
    fn from(error: room_settings::RoomSettingsError) -> Self {
        use room_settings::RoomSettingsError as Settings;
        match error {
            Settings::Cancelled => Self::Cancelled,
            Settings::AccountChanged => Self::AccountChanged,
            Settings::NotJoined => Self::NotJoined,
            Settings::Unavailable => Self::Unavailable,
            Settings::RateLimited => Self::RateLimited,
            Settings::TimedOut => Self::TimedOut,
            Settings::PermissionDenied => Self::InsufficientPower,
            _ => Self::InvalidState,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AdminMembership {
    Join,
    Invite,
    Knock,
    Ban,
}

/// A member's effective level. A privileged creator (room version 12 and
/// later) outranks every integer level, and nobody can change it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemberPower {
    Level(i64),
    Creator,
}

/// Actions the selected account may take on one member, from fresh room
/// state. The server repeats these checks when the change is sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemberActions {
    pub kick: bool,
    pub ban: bool,
    pub unban: bool,
    pub change_power: bool,
    /// Invite a member who knocked.
    pub approve: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminMember {
    pub user_id: String,
    pub display_name: Option<String>,
    pub membership: AdminMembership,
    pub power: MemberPower,
    /// Set in the room's per-user levels rather than inherited from the default.
    pub explicit_level: bool,
    pub is_self: bool,
    pub actions: MemberActions,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OwnAdminPermissions {
    pub invite: bool,
    pub kick: bool,
    pub ban: bool,
    pub unban: bool,
    pub change_power_levels: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerThresholds {
    pub users_default: i64,
    pub invite: i64,
    pub kick: i64,
    pub ban: i64,
    pub redact: i64,
    pub change_power_levels: i64,
}

/// A room-wide threshold in `m.room.power_levels`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PermissionKey {
    /// The level of members without a level of their own.
    UsersDefault,
    /// Messages and other events without a rule of their own.
    EventsDefault,
    Invite,
    Kick,
    Ban,
    /// Removing other members' messages.
    Redact,
    RoomName,
    RoomTopic,
    RoomAvatar,
    JoinRules,
    PowerLevels,
    /// Notifying everyone with `@room`.
    RoomNotification,
}

/// Every threshold the room-administration view shows, in display order.
pub const PERMISSION_KEYS: [PermissionKey; 12] = [
    PermissionKey::UsersDefault,
    PermissionKey::EventsDefault,
    PermissionKey::Invite,
    PermissionKey::Kick,
    PermissionKey::Ban,
    PermissionKey::Redact,
    PermissionKey::RoomName,
    PermissionKey::RoomTopic,
    PermissionKey::RoomAvatar,
    PermissionKey::JoinRules,
    PermissionKey::PowerLevels,
    PermissionKey::RoomNotification,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PermissionSetting {
    pub key: PermissionKey,
    /// The effective level, including inherited defaults.
    pub level: i64,
    /// The selected account may change this threshold under the current rules.
    pub editable: bool,
}

/// Who can read new messages, as `m.room.history_visibility`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryChoice {
    /// Members, from the point they joined.
    Joined,
    /// Members, from the point they were invited.
    Invited,
    /// Members, including messages from before they joined.
    Shared,
    /// Anyone, including people who have not joined.
    WorldReadable,
}

impl HistoryChoice {
    fn wire(self) -> HistoryVisibility {
        match self {
            Self::Joined => HistoryVisibility::Joined,
            Self::Invited => HistoryVisibility::Invited,
            Self::Shared => HistoryVisibility::Shared,
            Self::WorldReadable => HistoryVisibility::WorldReadable,
        }
    }
}

/// History visibility, guest access and encryption from fresh state. A value
/// is `None` when the room uses content a typed rewrite would lose; it stays
/// read-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessSettings {
    pub history: Option<HistoryChoice>,
    pub history_level: i64,
    pub history_editable: bool,
    pub guests_can_join: Option<bool>,
    pub guests_level: i64,
    pub guests_editable: bool,
    /// The room has an `m.room.encryption` state event. Matrix offers no way
    /// to turn encryption off again.
    pub encrypted: bool,
    pub encryption_level: i64,
    /// Encryption is off and the account may turn it on.
    pub encryption_editable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessChange {
    /// `from` is the value the operator reviewed.
    History {
        from: HistoryChoice,
        to: HistoryChoice,
    },
    Guests { allow: bool },
    /// Turn on end-to-end encryption; it cannot be turned off again.
    Encrypt,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MembershipCounts {
    pub joined: usize,
    pub invited: usize,
    pub knocking: usize,
    pub banned: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomAdminSnapshot {
    /// Joined first, then knocking, invited and banned; each by level, then name.
    pub members: Vec<AdminMember>,
    pub truncated: bool,
    pub counts: MembershipCounts,
    pub own_power: MemberPower,
    pub own: OwnAdminPermissions,
    /// `None` for a room version whose rules are unknown; every action is denied.
    pub thresholds: Option<PowerThresholds>,
    /// In `PERMISSION_KEYS` order; empty when the room's rules are unknown.
    pub permissions: Vec<PermissionSetting>,
    /// `None` when the room's rules are unknown.
    pub access: Option<AccessSettings>,
    pub room_version: String,
}

impl RoomAdminSnapshot {
    pub fn member(&self, user_id: &str) -> Option<&AdminMember> {
        self.members.iter().find(|row| row.user_id == user_id)
    }
    pub fn permission(&self, key: PermissionKey) -> Option<PermissionSetting> {
        self.permissions.iter().copied().find(|row| row.key == key)
    }
}

/// What the operator saw when confirming. A write is refused when the
/// server's current state for the target differs from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemberExpectation {
    /// `None` when the user has no listed membership.
    pub membership: Option<AdminMembership>,
    /// `None` skips the level comparison (an invitation by user ID).
    pub power: Option<MemberPower>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminAction {
    Invite,
    Kick,
    Ban,
    Unban,
    SetPowerLevel(i64),
    /// A room-wide threshold. The request's user ID is ignored and its
    /// expected power is the level the operator reviewed.
    SetPermission(PermissionKey, i64),
    /// History visibility or guest access. The request's user ID is ignored.
    SetAccess(AccessChange),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminRequest {
    pub action: AdminAction,
    pub user_id: String,
    pub expected: MemberExpectation,
    pub reason: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservedMembership {
    Join,
    Invite,
    Knock,
    Ban,
    Leave,
    /// The room has no membership state for this user.
    None,
}

impl ObservedMembership {
    pub fn listed(self) -> Option<AdminMembership> {
        match self {
            Self::Join => Some(AdminMembership::Join),
            Self::Invite => Some(AdminMembership::Invite),
            Self::Knock => Some(AdminMembership::Knock),
            Self::Ban => Some(AdminMembership::Ban),
            Self::Leave | Self::None => None,
        }
    }
}

/// The server's state for the target, read after the server accepted a write.
/// For a threshold change, `power` is the threshold's level and `membership`
/// is `None`. For an access change it only records that the read-back
/// succeeded; the new value is in the outcome's snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservedTarget {
    pub membership: ObservedMembership,
    pub power: MemberPower,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminOutcome {
    /// `None` when the server accepted the change but reading it back failed.
    pub observed: Option<ObservedTarget>,
    /// The read-back shows exactly the requested result.
    pub confirmed: bool,
    pub snapshot: Arc<RoomAdminSnapshot>,
}

#[derive(Clone)]
struct Override {
    membership: ObservedMembership,
    event_id: OwnedEventId,
    display_name: Option<String>,
    recorded: Instant,
}

struct RosterEntry {
    user_id: OwnedUserId,
    display_name: Option<String>,
    membership: AdminMembership,
    event_id: Option<OwnedEventId>,
    fallback_power: MemberPower,
}

pub struct RoomAdminSession {
    selection: AccountSelection,
    cancellation: RoomSettingsCancellation,
    room: Room,
    own_user: OwnedUserId,
    create: StateEnvelope,
    room_version: String,
    loaded: RwLock<Arc<RoomAdminSnapshot>>,
    overrides: std::sync::Mutex<HashMap<OwnedUserId, Override>>,
    operation: Mutex<()>,
}

impl AccountManager {
    /// Open member administration for one joined room of the selected account.
    /// Cancelling the token, or any account change, revokes the session and
    /// every pending operation; a request already sent may still apply.
    pub async fn open_room_admin_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        cancellation: RoomSettingsCancellation,
    ) -> Result<RoomAdminSession> {
        let (room, own_user) = guarded(selection, &cancellation, READ_TIMEOUT, async {
            let state = self.state.lock().await;
            let current = self
                .account_selection(selection.account_id())
                .ok_or(RoomAdminError::AccountChanged)?;
            if !selection.same_epoch(&current) {
                return Err(RoomAdminError::AccountChanged);
            }
            let room = account_room(&state, selection.account_id(), room_id)
                .map_err(|_| RoomAdminError::Unavailable)?;
            room_settings::ensure_joined(&room)?;
            let own_user = room
                .client()
                .user_id()
                .ok_or(RoomAdminError::Unavailable)?
                .to_owned();
            Ok((room, own_user))
        })
        .await?;
        let create = guarded(selection, &cancellation, READ_TIMEOUT, async {
            room_settings::state(&room, StateEventType::RoomCreate, "")
                .await?
                .ok_or(RoomAdminError::InvalidState)
        })
        .await?;
        let creation: RoomCreateEventContent = room_settings::content(&create)?;
        let session = RoomAdminSession {
            selection: selection.clone(),
            cancellation,
            room,
            own_user,
            create,
            room_version: creation.room_version.as_str().to_owned(),
            loaded: RwLock::new(Arc::new(RoomAdminSnapshot {
                members: Vec::new(),
                truncated: false,
                counts: MembershipCounts::default(),
                own_power: MemberPower::Level(0),
                own: OwnAdminPermissions::default(),
                thresholds: None,
                permissions: Vec::new(),
                access: None,
                room_version: String::new(),
            })),
            overrides: std::sync::Mutex::new(HashMap::new()),
            operation: Mutex::new(()),
        };
        session.refresh().await?;
        Ok(session)
    }
}

impl RoomAdminSession {
    pub fn account_id(&self) -> &str {
        self.selection.account_id()
    }
    pub fn room_id(&self) -> &str {
        self.room.room_id().as_str()
    }
    pub fn own_user_id(&self) -> &str {
        self.own_user.as_str()
    }
    pub fn snapshot(&self) -> Arc<RoomAdminSnapshot> {
        self.loaded
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    /// Sync updates of this room that may change its roster or roles.
    pub fn watch(&self) -> RosterWatch {
        RosterWatch {
            updates: self.room.subscribe_to_updates(),
        }
    }
    fn check(&self) -> Result<()> {
        room_settings::ensure_scope(&self.selection, &self.cancellation)?;
        room_settings::ensure_joined(&self.room)?;
        Ok(())
    }
    async fn run<T>(&self, limit: Duration, work: impl Future<Output = Result<T>>) -> Result<T> {
        guarded(&self.selection, &self.cancellation, limit, async {
            let _operation = self.operation.lock().await;
            self.check()?;
            work.await
        })
        .await
    }
    fn publish(&self, snapshot: RoomAdminSnapshot) -> Result<Arc<RoomAdminSnapshot>> {
        self.check()?;
        let snapshot = Arc::new(snapshot);
        *self.loaded.write().unwrap_or_else(|e| e.into_inner()) = snapshot.clone();
        Ok(snapshot)
    }
    async fn power_state(&self) -> Result<Option<StateEnvelope>> {
        let state = room_settings::state(&self.room, StateEventType::RoomPowerLevels, "").await?;
        Ok(state)
    }
    async fn member_state(&self, user: &UserId) -> Result<Option<StateEnvelope>> {
        let key = user.as_str();
        let state = room_settings::state(&self.room, StateEventType::RoomMember, key).await?;
        Ok(state)
    }
    async fn build(&self, power: Option<&StateEnvelope>) -> Result<RoomAdminSnapshot> {
        let levels = room_settings::effective_power_levels(&self.create, power)?;
        let listed = RoomMemberships::JOIN | RoomMemberships::INVITE | RoomMemberships::KNOCK;
        let members = self
            .room
            .members(listed | RoomMemberships::BAN)
            .await
            .map_err(|_| RoomAdminError::Unavailable)?;
        self.check()?;
        let rows = members.iter().filter_map(roster_entry).collect();
        let rows = {
            let mut overrides = self.overrides.lock().unwrap_or_else(|e| e.into_inner());
            apply_overrides(rows, &mut overrides, Instant::now())
        };
        let mut snapshot = assemble(&self.own_user, levels.as_ref(), rows, &self.room_version);
        if let Some(levels) = &levels {
            let history = self.optional_state(StateEventType::RoomHistoryVisibility).await?;
            let guests = self.optional_state(StateEventType::RoomGuestAccess).await?;
            let encryption = self.optional_state(StateEventType::RoomEncryption).await?;
            self.check()?;
            let (history, guests) = (history.as_ref(), guests.as_ref());
            let encrypted = encryption.is_some();
            let access = access_settings(levels, &self.own_user, history, guests, encrypted);
            snapshot.access = Some(access);
        }
        Ok(snapshot)
    }
    async fn optional_state(&self, kind: StateEventType) -> Result<Option<StateEnvelope>> {
        let state = room_settings::state(&self.room, kind, "").await?;
        Ok(state)
    }
    /// Re-read the roles and the synced roster.
    pub async fn refresh(&self) -> Result<Arc<RoomAdminSnapshot>> {
        self.run(READ_TIMEOUT, async {
            let power = self.power_state().await?;
            let snapshot = self.build(power.as_ref()).await?;
            self.publish(snapshot)
        })
        .await
    }
    /// Send one change after fresh precondition and permission checks, then
    /// read the affected state back. Never retried; a failure may still have
    /// reached the homeserver, so callers must refresh rather than resend.
    pub async fn apply(&self, request: AdminRequest) -> Result<AdminOutcome> {
        if let AdminAction::SetPermission(key, level) = request.action {
            return self.set_permission(key, level, request.expected.power).await;
        }
        if let AdminAction::SetAccess(change) = request.action {
            return self.set_access(change).await;
        }
        let target = parse_target(&request.user_id)?;
        let reason = normalize_reason(request.reason.as_deref())?;
        if let AdminAction::SetPowerLevel(level) = request.action {
            Int::try_from(level).map_err(|_| RoomAdminError::InvalidPowerLevel)?;
        }
        self.run(WRITE_TIMEOUT, async {
            let power = self.power_state().await?;
            let levels = room_settings::effective_power_levels(&self.create, power.as_ref())?
                .ok_or(RoomAdminError::InvalidState)?;
            let current = observed(self.member_state(&target).await?.as_ref())?;
            precondition(
                &levels,
                &self.own_user,
                &target,
                request.action,
                &request.expected,
                current,
            )?;
            self.check()?;
            self.send(&target, request.action, reason, &levels, power.as_ref())
                .await?;
            if request.action == AdminAction::Invite {
                // Mirror the SDK: the next encrypted send reloads members so
                // the invitee receives room keys.
                self.room.mark_members_missing();
            }
            self.check()?;
            // The server accepted the change. A failed read-back is reported
            // as unconfirmed, never as a failure or a success.
            let observed = self.read_back(&target, request.action, power).await.ok();
            let snapshot = match &observed {
                Some((_, power)) => match self.build(power.as_ref()).await {
                    Ok(built) => self.publish(built)?,
                    Err(_) => self.snapshot(),
                },
                None => self.snapshot(),
            };
            let observed = observed.map(|(target, _)| target);
            Ok(AdminOutcome {
                confirmed: observed.is_some_and(|seen| confirms(request.action, seen)),
                observed,
                snapshot,
            })
        })
        .await
    }
    /// Change one room-wide threshold. Matrix rejects a change unless the
    /// sender may send power levels and both the current and the new value
    /// are at most the sender's own level; the same rule is checked first.
    async fn set_permission(
        &self,
        key: PermissionKey,
        level: i64,
        reviewed: Option<MemberPower>,
    ) -> Result<AdminOutcome> {
        let new = Int::try_from(level).map_err(|_| RoomAdminError::InvalidPowerLevel)?;
        self.run(WRITE_TIMEOUT, async {
            let power = self.power_state().await?;
            let levels = room_settings::effective_power_levels(&self.create, power.as_ref())?
                .ok_or(RoomAdminError::InvalidState)?;
            let current = i64::from(permission_level(&levels, key));
            permission_precondition(&levels, &self.own_user, current, level, reviewed)?;
            let content = permission_content(&levels, power.as_ref(), key, new)?;
            self.check()?;
            let request = send_state_event::v3::Request::new_raw(
                self.room.room_id().to_owned(),
                StateEventType::RoomPowerLevels,
                String::new(),
                content,
            );
            self.room
                .client()
                .send(request)
                .with_request_config(write_config())
                .await
                .map_err(write_failure)?;
            self.check()?;
            // Accepted by the server; a failed read-back is unconfirmed, not failed.
            let observed = match self.power_state().await {
                Ok(power) => match self.build(power.as_ref()).await {
                    Ok(built) => {
                        let seen = built.permission(key).map(|row| row.level);
                        let snapshot = self.publish(built)?;
                        seen.map(|seen| (seen, snapshot))
                    }
                    Err(_) => None,
                },
                Err(_) => None,
            };
            let (observed, snapshot) = match observed {
                Some((seen, snapshot)) => (Some(seen), snapshot),
                None => (None, self.snapshot()),
            };
            let observed = observed.map(|seen| ObservedTarget {
                membership: ObservedMembership::None,
                power: MemberPower::Level(seen),
            });
            let action = AdminAction::SetPermission(key, level);
            Ok(AdminOutcome {
                confirmed: observed.is_some_and(|seen| confirms(action, seen)),
                observed,
                snapshot,
            })
        })
        .await
    }
    /// Change history visibility or guest access after checking that the
    /// reviewed value is still current and the account may send that state.
    async fn set_access(&self, change: AccessChange) -> Result<AdminOutcome> {
        if matches!(change, AccessChange::History { from, to } if from == to) {
            return Err(RoomAdminError::NotApplicable);
        }
        self.run(WRITE_TIMEOUT, async {
            let power = self.power_state().await?;
            let levels = room_settings::effective_power_levels(&self.create, power.as_ref())?
                .ok_or(RoomAdminError::InvalidState)?;
            let current = self.optional_state(access_kind(change)).await?;
            if !access_matches(change, current.as_ref()) {
                return Err(RoomAdminError::TargetChanged);
            }
            if !levels.user_can_send_state(&self.own_user, access_kind(change)) {
                return Err(RoomAdminError::InsufficientPower);
            }
            let request = access_request(self.room.room_id(), change)?;
            self.check()?;
            self.room
                .client()
                .send(request)
                .with_request_config(write_config())
                .await
                .map_err(write_failure)?;
            self.check()?;
            // Accepted by the server; a failed read-back is unconfirmed, not failed.
            let built = match self.build(power.as_ref()).await {
                Ok(built) => Some(self.publish(built)?),
                Err(_) => None,
            };
            let confirmed = built
                .as_ref()
                .and_then(|snapshot| snapshot.access)
                .is_some_and(|access| access_applied(change, access));
            let read_back = ObservedTarget {
                membership: ObservedMembership::None,
                power: MemberPower::Level(0),
            };
            let observed = built.is_some().then_some(read_back);
            Ok(AdminOutcome {
                confirmed,
                observed,
                snapshot: built.unwrap_or_else(|| self.snapshot()),
            })
        })
        .await
    }
    async fn send(
        &self,
        target: &UserId,
        action: AdminAction,
        reason: Option<String>,
        levels: &RoomPowerLevels,
        power: Option<&StateEnvelope>,
    ) -> Result<()> {
        let room_id = self.room.room_id().to_owned();
        let client = self.room.client();
        let sent = match action {
            AdminAction::Invite => {
                let mut recipient = InviteUserId::new(target.to_owned());
                recipient.reason = reason;
                let request =
                    invite_user::v3::Request::new(room_id, InvitationRecipient::UserId(recipient));
                client
                    .send(request)
                    .with_request_config(write_config())
                    .await
                    .map(drop)
            }
            AdminAction::Kick => {
                let mut request = kick_user::v3::Request::new(room_id, target.to_owned());
                request.reason = reason;
                client
                    .send(request)
                    .with_request_config(write_config())
                    .await
                    .map(drop)
            }
            AdminAction::Ban => {
                let mut request = ban_user::v3::Request::new(room_id, target.to_owned());
                request.reason = reason;
                client
                    .send(request)
                    .with_request_config(write_config())
                    .await
                    .map(drop)
            }
            AdminAction::Unban => {
                let mut request = unban_user::v3::Request::new(room_id, target.to_owned());
                request.reason = reason;
                client
                    .send(request)
                    .with_request_config(write_config())
                    .await
                    .map(drop)
            }
            AdminAction::SetPowerLevel(level) => {
                let level = Int::try_from(level).map_err(|_| RoomAdminError::InvalidPowerLevel)?;
                let content = member_level_content(levels, power, target, level)?;
                let request = send_state_event::v3::Request::new_raw(
                    room_id,
                    StateEventType::RoomPowerLevels,
                    String::new(),
                    content,
                );
                client
                    .send(request)
                    .with_request_config(write_config())
                    .await
                    .map(drop)
            }
            AdminAction::SetPermission(..) | AdminAction::SetAccess(_) => {
                return Err(RoomAdminError::NotApplicable);
            }
        };
        sent.map_err(write_failure)
    }
    /// Read back exactly the state the action changed. Returns the observed
    /// target and the power-level state to rebuild the roster from.
    async fn read_back(
        &self,
        target: &UserId,
        action: AdminAction,
        previous_power: Option<StateEnvelope>,
    ) -> Result<(ObservedTarget, Option<StateEnvelope>)> {
        let power = match action {
            AdminAction::SetPowerLevel(_) => self.power_state().await?,
            _ => previous_power,
        };
        let member = self.member_state(target).await?;
        self.check()?;
        let membership = observed(member.as_ref())?;
        let levels = room_settings::effective_power_levels(&self.create, power.as_ref())?;
        let power_now = levels
            .as_ref()
            .map_or(MemberPower::Level(0), |rules| member_power(rules, target));
        if !matches!(action, AdminAction::SetPowerLevel(_))
            && let Some(event) = &member
        {
            let display_name = room_settings::content::<RoomMemberEventContent>(event)
                .ok()
                .and_then(|content| content.displayname)
                .map(|name| bounded(&name, MAX_DISPLAY_NAME_BYTES));
            let record = Override {
                membership,
                event_id: event.event_id.clone(),
                display_name,
                recorded: Instant::now(),
            };
            let mut overrides = self.overrides.lock().unwrap_or_else(|e| e.into_inner());
            record_override(&mut overrides, target.to_owned(), record);
        }
        let seen = ObservedTarget {
            membership,
            power: power_now,
        };
        Ok((seen, power))
    }
}

/// Signals sync updates of one room that may change its roster or roles.
pub struct RosterWatch {
    updates: broadcast::Receiver<RoomUpdate>,
}

impl RosterWatch {
    /// Wait for the next relevant update. Returns `false` once the client is
    /// gone. A lagging receiver reports a change rather than missing one.
    pub async fn changed(&mut self) -> bool {
        loop {
            match self.updates.recv().await {
                Ok(update) if affects_roster(&update) => return true,
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => return true,
                Err(broadcast::error::RecvError::Closed) => return false,
            }
        }
    }
}

fn affects_roster(update: &RoomUpdate) -> bool {
    let RoomUpdate::Joined { updates, .. } = update else {
        // Our own membership changed; the next read reports it.
        return true;
    };
    let state = match &updates.state {
        SyncState::Before(events) | SyncState::After(events) => events,
    };
    for event in state {
        if roster_event_type(event.get_field("type")) {
            return true;
        }
    }
    for event in &updates.timeline.events {
        if roster_event_type(event.raw().get_field("type")) {
            return true;
        }
    }
    false
}

fn roster_event_type(kind: serde_json::Result<Option<String>>) -> bool {
    matches!(
        kind.ok().flatten().as_deref(),
        Some("m.room.member" | "m.room.power_levels" | "m.room.create")
    )
}

async fn guarded<T>(
    selection: &AccountSelection,
    cancellation: &RoomSettingsCancellation,
    limit: Duration,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    room_settings::ensure_scope(selection, cancellation)?;
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(RoomAdminError::Cancelled),
        _ = selection.revoked() => Err(RoomAdminError::AccountChanged),
        result = time::timeout(limit, work) => result.map_err(|_| RoomAdminError::TimedOut)?,
    }?;
    room_settings::ensure_scope(selection, cancellation)?;
    Ok(result)
}

fn write_config() -> RequestConfig {
    RequestConfig::new().retry_limit(0).timeout(REQUEST_TIMEOUT)
}

// Keep only a fixed category. Never retain reflected response bodies, URLs or
// Retry-After text, and never resend a moderation write automatically.
fn write_failure(error: HttpError) -> RoomAdminError {
    let Some(error) = error.as_client_api_error() else {
        return RoomAdminError::Unavailable;
    };
    let status = error.status_code.as_u16();
    if status == 429 || matches!(error.error_kind(), Some(ErrorKind::LimitExceeded(_))) {
        RoomAdminError::RateLimited
    } else if status == 403 {
        RoomAdminError::Forbidden
    } else {
        RoomAdminError::Unavailable
    }
}

fn parse_target(user_id: &str) -> Result<OwnedUserId> {
    let user_id = user_id.trim();
    if user_id.is_empty() || user_id.len() > MAX_USER_ID_BYTES {
        return Err(RoomAdminError::InvalidUserId);
    }
    UserId::parse(user_id).map_err(|_| RoomAdminError::InvalidUserId)
}

fn normalize_reason(reason: Option<&str>) -> Result<Option<String>> {
    let Some(reason) = reason.map(str::trim).filter(|reason| !reason.is_empty()) else {
        return Ok(None);
    };
    if reason.len() > MAX_ADMIN_REASON_BYTES || reason.chars().any(char::is_control) {
        return Err(RoomAdminError::InvalidReason);
    }
    Ok(Some(reason.to_owned()))
}

fn bounded(text: &str, maximum: usize) -> String {
    if text.len() <= maximum {
        return text.to_owned();
    }
    let mut end = maximum;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn observed(event: Option<&StateEnvelope>) -> Result<ObservedMembership> {
    let Some(event) = event else {
        return Ok(ObservedMembership::None);
    };
    let content: RoomMemberEventContent = room_settings::content(event)?;
    Ok(match content.membership {
        MembershipState::Join => ObservedMembership::Join,
        MembershipState::Invite => ObservedMembership::Invite,
        MembershipState::Knock => ObservedMembership::Knock,
        MembershipState::Ban => ObservedMembership::Ban,
        MembershipState::Leave => ObservedMembership::Leave,
        _ => return Err(RoomAdminError::InvalidState),
    })
}

fn member_power(levels: &RoomPowerLevels, user: &UserId) -> MemberPower {
    match levels.for_user(user) {
        UserPowerLevel::Int(level) => MemberPower::Level(level.into()),
        _ => MemberPower::Creator,
    }
}

fn member_actions(
    levels: &RoomPowerLevels,
    own: &UserId,
    target: &UserId,
    membership: AdminMembership,
) -> MemberActions {
    use AdminMembership::{Ban, Invite, Join, Knock};
    let assignable = matches!(membership, Join | Invite)
        && levels.user_can_change_user_power_level(own, target);
    if own == target {
        return MemberActions {
            change_power: assignable,
            ..MemberActions::default()
        };
    }
    MemberActions {
        kick: matches!(membership, Join | Invite | Knock) && levels.user_can_kick_user(own, target),
        ban: membership != Ban && levels.user_can_ban_user(own, target),
        unban: membership == Ban && levels.user_can_unban_user(own, target),
        change_power: assignable,
        approve: membership == Knock && levels.user_can_invite(own),
    }
}

/// Refuse a change that the current authoritative state already rules out,
/// or that differs from what the operator reviewed.
fn precondition(
    levels: &RoomPowerLevels,
    own: &UserId,
    target: &UserId,
    action: AdminAction,
    expected: &MemberExpectation,
    current: ObservedMembership,
) -> Result<()> {
    use ObservedMembership::{Ban, Invite, Join, Knock, Leave};
    let target_power = member_power(levels, target);
    // An invitation by ID may target someone a bounded roster did not list.
    if action == AdminAction::Invite {
        match current {
            Join => return Err(RoomAdminError::AlreadyJoined),
            Invite => return Err(RoomAdminError::AlreadyInvited),
            Ban => return Err(RoomAdminError::TargetBanned),
            Knock | Leave | ObservedMembership::None => {}
        }
    }
    if expected.membership != current.listed()
        || expected.power.is_some_and(|power| power != target_power)
    {
        return Err(RoomAdminError::TargetChanged);
    }
    let removing = matches!(action, AdminAction::Kick | AdminAction::Ban);
    if removing && own == target {
        return Err(RoomAdminError::SelfTarget);
    }
    if target_power == MemberPower::Creator && action != AdminAction::Invite {
        return Err(RoomAdminError::CreatorProtected);
    }
    let allowed = match action {
        AdminAction::Invite => levels.user_can_invite(own),
        AdminAction::Kick => {
            if !matches!(current, Join | Invite | Knock) {
                return Err(RoomAdminError::NotApplicable);
            }
            levels.user_can_kick_user(own, target)
        }
        AdminAction::Ban => {
            if current == Ban {
                return Err(RoomAdminError::NotApplicable);
            }
            levels.user_can_ban_user(own, target)
        }
        AdminAction::Unban => {
            if current != Ban {
                return Err(RoomAdminError::NotApplicable);
            }
            levels.user_can_unban_user(own, target)
        }
        AdminAction::SetPowerLevel(level) => {
            if !matches!(current, Join | Invite) || target_power == MemberPower::Level(level) {
                return Err(RoomAdminError::NotApplicable);
            }
            // Matrix also rejects a new level above the sender's own level.
            levels.user_can_change_user_power_level(own, target)
                && MemberPower::Level(level) <= member_power(levels, own)
        }
        AdminAction::SetPermission(..) | AdminAction::SetAccess(_) => {
            return Err(RoomAdminError::NotApplicable);
        }
    };
    if allowed {
        Ok(())
    } else {
        Err(RoomAdminError::InsufficientPower)
    }
}

fn confirms(action: AdminAction, seen: ObservedTarget) -> bool {
    use ObservedMembership::{Ban, Invite, Join, Leave};
    match action {
        AdminAction::Invite => matches!(seen.membership, Invite | Join),
        AdminAction::Kick | AdminAction::Unban => seen.membership == Leave,
        AdminAction::Ban => seen.membership == Ban,
        AdminAction::SetPowerLevel(level) | AdminAction::SetPermission(_, level) => {
            seen.power == MemberPower::Level(level)
        }
        // Access changes are confirmed from the rebuilt snapshot instead.
        AdminAction::SetAccess(_) => false,
    }
}

/// The content's only field, when it is a string. Any other shape would be
/// lost by a typed rewrite, so the setting stays read-only.
fn single_string<'a>(content: &'a Value, key: &str) -> Option<&'a str> {
    let object = content.as_object()?;
    if object.len() != 1 {
        return None;
    }
    object.get(key)?.as_str()
}

fn history_choice(event: Option<&StateEnvelope>) -> Option<HistoryChoice> {
    // A room without the event behaves as `shared`.
    let Some(event) = event else {
        return Some(HistoryChoice::Shared);
    };
    match single_string(&event.content, "history_visibility")? {
        "joined" => Some(HistoryChoice::Joined),
        "invited" => Some(HistoryChoice::Invited),
        "shared" => Some(HistoryChoice::Shared),
        "world_readable" => Some(HistoryChoice::WorldReadable),
        _ => None,
    }
}

fn guests_can_join(event: Option<&StateEnvelope>) -> Option<bool> {
    // A room without the event forbids guests.
    let Some(event) = event else {
        return Some(false);
    };
    match single_string(&event.content, "guest_access")? {
        "can_join" => Some(true),
        "forbidden" => Some(false),
        _ => None,
    }
}

fn access_settings(
    levels: &RoomPowerLevels,
    own: &UserId,
    history: Option<&StateEnvelope>,
    guests: Option<&StateEnvelope>,
    encrypted: bool,
) -> AccessSettings {
    let history = history_choice(history);
    let guests_can_join = guests_can_join(guests);
    let history_kind = StateEventType::RoomHistoryVisibility;
    let guests_kind = StateEventType::RoomGuestAccess;
    let encryption_kind = StateEventType::RoomEncryption;
    AccessSettings {
        history,
        history_level: levels.for_state(history_kind.clone()).into(),
        history_editable: history.is_some() && levels.user_can_send_state(own, history_kind),
        guests_can_join,
        guests_level: levels.for_state(guests_kind.clone()).into(),
        guests_editable: guests_can_join.is_some() && levels.user_can_send_state(own, guests_kind),
        encrypted,
        encryption_level: levels.for_state(encryption_kind.clone()).into(),
        encryption_editable: !encrypted && levels.user_can_send_state(own, encryption_kind),
    }
}

fn access_kind(change: AccessChange) -> StateEventType {
    match change {
        AccessChange::History { .. } => StateEventType::RoomHistoryVisibility,
        AccessChange::Guests { .. } => StateEventType::RoomGuestAccess,
        AccessChange::Encrypt => StateEventType::RoomEncryption,
    }
}

fn access_matches(change: AccessChange, current: Option<&StateEnvelope>) -> bool {
    match change {
        AccessChange::History { from, .. } => history_choice(current) == Some(from),
        AccessChange::Guests { allow } => guests_can_join(current) == Some(!allow),
        // Any encryption event, even one this client cannot read, means it
        // is on already; it is never replaced.
        AccessChange::Encrypt => current.is_none(),
    }
}

fn access_applied(change: AccessChange, access: AccessSettings) -> bool {
    match change {
        AccessChange::History { to, .. } => access.history == Some(to),
        AccessChange::Guests { allow } => access.guests_can_join == Some(allow),
        AccessChange::Encrypt => access.encrypted,
    }
}

fn access_request(room: &RoomId, change: AccessChange) -> Result<send_state_event::v3::Request> {
    let room = room.to_owned();
    let request = match change {
        AccessChange::History { to, .. } => {
            let content = RoomHistoryVisibilityEventContent::new(to.wire());
            send_state_event::v3::Request::new(room, &EmptyStateKey, &content)
        }
        AccessChange::Guests { allow } => {
            let access = if allow {
                GuestAccess::CanJoin
            } else {
                GuestAccess::Forbidden
            };
            let content = RoomGuestAccessEventContent::new(access);
            send_state_event::v3::Request::new(room, &EmptyStateKey, &content)
        }
        AccessChange::Encrypt => {
            let content = RoomEncryptionEventContent::with_recommended_defaults();
            send_state_event::v3::Request::new(room, &EmptyStateKey, &content)
        }
    };
    request.map_err(|_| RoomAdminError::InvalidState)
}

fn permission_level(levels: &RoomPowerLevels, key: PermissionKey) -> Int {
    match key {
        PermissionKey::UsersDefault => levels.users_default,
        PermissionKey::EventsDefault => levels.events_default,
        PermissionKey::Invite => levels.invite,
        PermissionKey::Kick => levels.kick,
        PermissionKey::Ban => levels.ban,
        PermissionKey::Redact => levels.redact,
        PermissionKey::RoomName => levels.for_state(StateEventType::RoomName),
        PermissionKey::RoomTopic => levels.for_state(StateEventType::RoomTopic),
        PermissionKey::RoomAvatar => levels.for_state(StateEventType::RoomAvatar),
        PermissionKey::JoinRules => levels.for_state(StateEventType::RoomJoinRules),
        PermissionKey::PowerLevels => levels.for_state(StateEventType::RoomPowerLevels),
        PermissionKey::RoomNotification => levels.notifications.room,
    }
}

/// Where a threshold lives in the content: a top-level key, an entry of
/// `events`, or an entry of `notifications`.
fn permission_field(key: PermissionKey) -> (Option<&'static str>, &'static str) {
    match key {
        PermissionKey::UsersDefault => (None, "users_default"),
        PermissionKey::EventsDefault => (None, "events_default"),
        PermissionKey::Invite => (None, "invite"),
        PermissionKey::Kick => (None, "kick"),
        PermissionKey::Ban => (None, "ban"),
        PermissionKey::Redact => (None, "redact"),
        PermissionKey::RoomName => (Some("events"), "m.room.name"),
        PermissionKey::RoomTopic => (Some("events"), "m.room.topic"),
        PermissionKey::RoomAvatar => (Some("events"), "m.room.avatar"),
        PermissionKey::JoinRules => (Some("events"), "m.room.join_rules"),
        PermissionKey::PowerLevels => (Some("events"), "m.room.power_levels"),
        PermissionKey::RoomNotification => (Some("notifications"), "room"),
    }
}

/// The sender may send power levels and outranks both the current and the
/// new value (a privileged creator outranks every level).
fn can_set_permission(
    levels: &RoomPowerLevels,
    own: &UserId,
    current: i64,
    new: Option<i64>,
) -> bool {
    let own_power = member_power(levels, own);
    levels.user_can_send_state(own, StateEventType::RoomPowerLevels)
        && own_power >= MemberPower::Level(current)
        && new.is_none_or(|new| own_power >= MemberPower::Level(new))
}

fn permission_precondition(
    levels: &RoomPowerLevels,
    own: &UserId,
    current: i64,
    level: i64,
    reviewed: Option<MemberPower>,
) -> Result<()> {
    if reviewed.is_some_and(|reviewed| reviewed != MemberPower::Level(current)) {
        return Err(RoomAdminError::TargetChanged);
    }
    if current == level {
        return Err(RoomAdminError::NotApplicable);
    }
    if !can_set_permission(levels, own, current, Some(level)) {
        return Err(RoomAdminError::InsufficientPower);
    }
    Ok(())
}

/// The server's current content, or the effective defaults when the room has
/// no power-level state. Edits keep every other field, including unknown ones.
fn current_content(levels: &RoomPowerLevels, power: Option<&StateEnvelope>) -> Result<Value> {
    if let Some(event) = power {
        return Ok(event.content.clone());
    }
    let content = RoomPowerLevelsEventContent::try_from(levels.clone())
        .map_err(|_| RoomAdminError::InvalidState)?;
    serde_json::to_value(content).map_err(|_| RoomAdminError::InvalidState)
}

/// Set `key` in the content, or in its `section` object when one is given.
fn set_level(content: &mut Value, section: Option<&str>, key: &str, level: Int) -> Result<()> {
    let invalid = || RoomAdminError::InvalidState;
    let mut object = content.as_object_mut().ok_or_else(invalid)?;
    if let Some(section) = section {
        let empty = Value::Object(serde_json::Map::new());
        object = object
            .entry(section)
            .or_insert(empty)
            .as_object_mut()
            .ok_or_else(invalid)?;
    }
    object.insert(key.to_owned(), Value::from(i64::from(level)));
    Ok(())
}

fn finish_content(content: &Value) -> Result<Raw<AnyStateEventContent>> {
    let text = serde_json::to_string(content).map_err(|_| RoomAdminError::InvalidState)?;
    if text.len() > MAX_POWER_LEVELS_BYTES {
        return Err(RoomAdminError::InvalidState);
    }
    Raw::from_json_string(text).map_err(|_| RoomAdminError::InvalidState)
}

/// Change one user's level in the server's current content.
fn member_level_content(
    levels: &RoomPowerLevels,
    power: Option<&StateEnvelope>,
    target: &UserId,
    level: Int,
) -> Result<Raw<AnyStateEventContent>> {
    let mut content = current_content(levels, power)?;
    set_level(&mut content, Some("users"), target.as_str(), level)?;
    finish_content(&content)
}

/// Change one room-wide threshold in the server's current content.
fn permission_content(
    levels: &RoomPowerLevels,
    power: Option<&StateEnvelope>,
    key: PermissionKey,
    level: Int,
) -> Result<Raw<AnyStateEventContent>> {
    let mut content = current_content(levels, power)?;
    let (section, field) = permission_field(key);
    set_level(&mut content, section, field, level)?;
    finish_content(&content)
}

fn roster_entry(member: &RoomMember) -> Option<RosterEntry> {
    let membership = match member.membership() {
        MembershipState::Join => AdminMembership::Join,
        MembershipState::Invite => AdminMembership::Invite,
        MembershipState::Knock => AdminMembership::Knock,
        MembershipState::Ban => AdminMembership::Ban,
        _ => return None,
    };
    let event_id = match &**member.event() {
        MemberEvent::Sync(event) => Some(event.event_id().to_owned()),
        MemberEvent::Stripped(_) => None,
    };
    Some(RosterEntry {
        user_id: member.user_id().to_owned(),
        display_name: member
            .display_name()
            .map(|name| bounded(name, MAX_DISPLAY_NAME_BYTES)),
        membership,
        event_id,
        fallback_power: match member.power_level() {
            UserPowerLevel::Int(level) => MemberPower::Level(level.into()),
            _ => MemberPower::Creator,
        },
    })
}

fn record_override(
    overrides: &mut HashMap<OwnedUserId, Override>,
    user: OwnedUserId,
    record: Override,
) {
    if overrides.len() >= MAX_OVERRIDES && !overrides.contains_key(&user) {
        let oldest = overrides
            .iter()
            .min_by_key(|(_, record)| record.recorded)
            .map(|(user, _)| user.clone());
        if let Some(oldest) = oldest {
            overrides.remove(&oldest);
        }
    }
    overrides.insert(user, record);
}

/// Show verified writes until the synced store holds the same member event.
fn apply_overrides(
    entries: Vec<RosterEntry>,
    overrides: &mut HashMap<OwnedUserId, Override>,
    now: Instant,
) -> Vec<RosterEntry> {
    overrides.retain(|_, record| now.saturating_duration_since(record.recorded) < OVERRIDE_TTL);
    let mut seen = HashSet::new();
    let mut result = Vec::with_capacity(entries.len());
    for mut entry in entries {
        if let Some(record) = overrides.get(&entry.user_id) {
            if entry.event_id.as_ref() == Some(&record.event_id) {
                overrides.remove(&entry.user_id);
            } else {
                seen.insert(entry.user_id.clone());
                let Some(membership) = record.membership.listed() else {
                    continue;
                };
                entry.membership = membership;
            }
        }
        result.push(entry);
    }
    for (user_id, record) in overrides.iter() {
        if seen.contains(user_id) {
            continue;
        }
        if let Some(membership) = record.membership.listed() {
            result.push(RosterEntry {
                user_id: user_id.clone(),
                display_name: record.display_name.clone(),
                membership,
                event_id: Some(record.event_id.clone()),
                fallback_power: MemberPower::Level(0),
            });
        }
    }
    result
}

fn assemble(
    own: &UserId,
    levels: Option<&RoomPowerLevels>,
    entries: Vec<RosterEntry>,
    room_version: &str,
) -> RoomAdminSnapshot {
    let mut counts = MembershipCounts::default();
    let mut own_fallback = MemberPower::Level(0);
    let mut members = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry.membership {
            AdminMembership::Join => counts.joined += 1,
            AdminMembership::Invite => counts.invited += 1,
            AdminMembership::Knock => counts.knocking += 1,
            AdminMembership::Ban => counts.banned += 1,
        }
        let is_self = &*entry.user_id == own;
        if is_self {
            own_fallback = entry.fallback_power;
        }
        let (power, explicit_level, actions) = match levels {
            Some(levels) => (
                member_power(levels, &entry.user_id),
                levels.users.contains_key(&entry.user_id),
                member_actions(levels, own, &entry.user_id, entry.membership),
            ),
            None => (entry.fallback_power, false, MemberActions::default()),
        };
        members.push(AdminMember {
            user_id: entry.user_id.to_string(),
            display_name: entry.display_name,
            membership: entry.membership,
            power,
            explicit_level,
            is_self,
            actions,
        });
    }
    members.sort_by_cached_key(|member| {
        let rank = match member.membership {
            AdminMembership::Join => 0u8,
            AdminMembership::Knock => 1,
            AdminMembership::Invite => 2,
            AdminMembership::Ban => 3,
        };
        let name = member
            .display_name
            .as_deref()
            .unwrap_or(&member.user_id)
            .to_lowercase();
        (rank, Reverse(member.power), name, member.user_id.clone())
    });
    let truncated = members.len() > MAX_ROSTER_MEMBERS;
    members.truncate(MAX_ROSTER_MEMBERS);
    let permissions = levels.map_or_else(Vec::new, |levels| {
        PERMISSION_KEYS
            .iter()
            .map(|&key| {
                let level = i64::from(permission_level(levels, key));
                let editable = can_set_permission(levels, own, level, None);
                PermissionSetting {
                    key,
                    level,
                    editable,
                }
            })
            .collect()
    });
    let (own_power, own_permissions, thresholds) = match levels {
        Some(levels) => (
            member_power(levels, own),
            OwnAdminPermissions {
                invite: levels.user_can_invite(own),
                kick: levels.user_can_kick(own),
                ban: levels.user_can_ban(own),
                unban: levels.user_can_unban(own),
                change_power_levels: levels
                    .user_can_send_state(own, StateEventType::RoomPowerLevels),
            },
            Some(PowerThresholds {
                users_default: levels.users_default.into(),
                invite: levels.invite.into(),
                kick: levels.kick.into(),
                ban: levels.ban.into(),
                redact: levels.redact.into(),
                change_power_levels: levels.for_state(StateEventType::RoomPowerLevels).into(),
            }),
        ),
        None => (own_fallback, OwnAdminPermissions::default(), None),
    };
    RoomAdminSnapshot {
        members,
        truncated,
        counts,
        own_power,
        own: own_permissions,
        thresholds,
        permissions,
        access: None,
        room_version: room_version.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_selection::Selections;
    use matrix_sdk::ruma::{EventId, owned_event_id, owned_user_id, room_id, user_id};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn envelope(kind: &str, sender: &str, content: Value) -> StateEnvelope {
        serde_json::from_value(json!({
            "event_id": "$fixture", "room_id": "!admin:example.org", "sender": sender,
            "type": kind, "state_key": "", "content": content
        }))
        .unwrap()
    }
    fn levels(version: &str, users: Value) -> RoomPowerLevels {
        let create = envelope(
            "m.room.create",
            "@casey:example.org",
            json!({"room_version": version, "creator": "@casey:example.org"}),
        );
        let power = envelope(
            "m.room.power_levels",
            "@casey:example.org",
            json!({"users": users, "kick": 50, "ban": 50, "invite": 0, "state_default": 50}),
        );
        room_settings::effective_power_levels(&create, Some(&power))
            .unwrap()
            .unwrap()
    }
    fn expect(membership: Option<AdminMembership>, level: i64) -> MemberExpectation {
        MemberExpectation {
            membership,
            power: Some(MemberPower::Level(level)),
        }
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[test]
    fn moderation_needs_a_fresh_level_above_the_target_and_never_targets_self() {
        let levels = levels(
            "11",
            json!({"@admin:example.org": 100, "@mod:example.org": 50, "@peer:example.org": 50}),
        );
        let own = user_id!("@mod:example.org");
        let member = user_id!("@member:example.org");
        let joined = expect(Some(AdminMembership::Join), 0);
        let check = |target: &UserId, action, expected: &MemberExpectation, current| {
            precondition(&levels, own, target, action, expected, current)
        };
        let join = ObservedMembership::Join;
        let left = ObservedMembership::Leave;
        let ban = ObservedMembership::Ban;
        assert_eq!(check(member, AdminAction::Kick, &joined, join), Ok(()));
        assert_eq!(check(member, AdminAction::Ban, &joined, join), Ok(()));
        for (target, level) in [("@admin:example.org", 100), ("@peer:example.org", 50)] {
            let target = UserId::parse(target).unwrap();
            let expected = expect(Some(AdminMembership::Join), level);
            for action in [AdminAction::Kick, AdminAction::Ban] {
                assert_eq!(
                    check(&target, action, &expected, join),
                    Err(RoomAdminError::InsufficientPower)
                );
            }
        }
        let own_row = expect(Some(AdminMembership::Join), 50);
        for action in [AdminAction::Kick, AdminAction::Ban] {
            assert_eq!(
                check(own, action, &own_row, join),
                Err(RoomAdminError::SelfTarget)
            );
        }
        // The reviewed row must still describe the server's current state.
        let invited = expect(Some(AdminMembership::Invite), 0);
        assert_eq!(
            check(member, AdminAction::Kick, &invited, join),
            Err(RoomAdminError::TargetChanged)
        );
        let promoted = expect(Some(AdminMembership::Join), 50);
        assert_eq!(
            check(member, AdminAction::Kick, &promoted, join),
            Err(RoomAdminError::TargetChanged)
        );
        assert_eq!(
            check(member, AdminAction::Unban, &joined, join),
            Err(RoomAdminError::NotApplicable)
        );
        let banned = expect(Some(AdminMembership::Ban), 0);
        assert_eq!(check(member, AdminAction::Unban, &banned, ban), Ok(()));
        let absent = MemberExpectation {
            membership: None,
            power: None,
        };
        assert_eq!(check(member, AdminAction::Invite, &absent, left), Ok(()));
        assert_eq!(
            check(member, AdminAction::Invite, &absent, ban),
            Err(RoomAdminError::TargetBanned)
        );
        // A request to join that was withdrawn is not approved by an invite.
        let knocked = expect(Some(AdminMembership::Knock), 0);
        assert_eq!(
            check(member, AdminAction::Invite, &knocked, left),
            Err(RoomAdminError::TargetChanged)
        );
    }

    #[test]
    fn levels_never_exceed_the_senders_own_and_privileged_creators_are_immutable() {
        let levels = levels("11", json!({"@mod:example.org": 50}));
        let own = user_id!("@mod:example.org");
        let member = user_id!("@member:example.org");
        let joined = expect(Some(AdminMembership::Join), 0);
        let join = ObservedMembership::Join;
        let set = |target: &UserId, level, expected: &MemberExpectation| {
            let action = AdminAction::SetPowerLevel(level);
            precondition(&levels, own, target, action, expected, join)
        };
        assert_eq!(set(member, 50, &joined), Ok(()));
        assert_eq!(
            set(member, 51, &joined),
            Err(RoomAdminError::InsufficientPower)
        );
        assert_eq!(set(member, 0, &joined), Err(RoomAdminError::NotApplicable));
        let own_row = expect(Some(AdminMembership::Join), 50);
        assert_eq!(set(own, 0, &own_row), Ok(()));
        assert_eq!(
            set(own, 60, &own_row),
            Err(RoomAdminError::InsufficientPower)
        );

        let v12 = levels_v12();
        let own = user_id!("@admin:example.org");
        let creator = user_id!("@casey:example.org");
        let expected = MemberExpectation {
            membership: Some(AdminMembership::Join),
            power: Some(MemberPower::Creator),
        };
        for action in [
            AdminAction::Kick,
            AdminAction::Ban,
            AdminAction::SetPowerLevel(0),
        ] {
            assert_eq!(
                precondition(&v12, own, creator, action, &expected, join),
                Err(RoomAdminError::CreatorProtected)
            );
        }
        let actions = member_actions(&v12, own, creator, AdminMembership::Join);
        assert_eq!(actions, MemberActions::default());
    }

    fn levels_v12() -> RoomPowerLevels {
        let create = envelope(
            "m.room.create",
            "@casey:example.org",
            json!({"room_version": "12"}),
        );
        let power = envelope(
            "m.room.power_levels",
            "@casey:example.org",
            json!({"users": {"@admin:example.org": 100}}),
        );
        room_settings::effective_power_levels(&create, Some(&power))
            .unwrap()
            .unwrap()
    }

    fn entry(user: &str, membership: AdminMembership, event: &str) -> RosterEntry {
        RosterEntry {
            user_id: UserId::parse(user).unwrap(),
            display_name: None,
            membership,
            event_id: Some(EventId::parse(event).unwrap()),
            fallback_power: MemberPower::Level(0),
        }
    }

    #[test]
    fn verified_writes_show_until_sync_stores_the_same_event_or_expire() {
        let recorded = Instant::now();
        let mut overrides = HashMap::new();
        record_override(
            &mut overrides,
            owned_user_id!("@target:example.org"),
            Override {
                membership: ObservedMembership::Ban,
                event_id: owned_event_id!("$ban"),
                display_name: None,
                recorded,
            },
        );
        let stale = || vec![entry("@target:example.org", AdminMembership::Join, "$join")];
        let shown = apply_overrides(stale(), &mut overrides, recorded);
        assert_eq!(shown[0].membership, AdminMembership::Ban);
        let synced = vec![entry("@target:example.org", AdminMembership::Ban, "$ban")];
        apply_overrides(synced, &mut overrides, recorded);
        assert!(overrides.is_empty(), "the synced store caught up");

        record_override(
            &mut overrides,
            owned_user_id!("@target:example.org"),
            Override {
                membership: ObservedMembership::Leave,
                event_id: owned_event_id!("$kick"),
                display_name: None,
                recorded,
            },
        );
        assert!(apply_overrides(stale(), &mut overrides, recorded).is_empty());
        let later = recorded + OVERRIDE_TTL;
        let shown = apply_overrides(stale(), &mut overrides, later);
        assert_eq!(shown[0].membership, AdminMembership::Join);
        assert!(overrides.is_empty(), "an expired record defers to sync");
    }

    #[test]
    fn unknown_room_rules_deny_every_action_and_own_row_can_only_change_level() {
        let own = user_id!("@mod:example.org");
        let rows = vec![
            entry("@mod:example.org", AdminMembership::Join, "$a"),
            entry("@member:example.org", AdminMembership::Knock, "$b"),
        ];
        let unknown = assemble(own, None, rows, "org.example.future");
        assert!(unknown.thresholds.is_none());
        assert_eq!(unknown.own, OwnAdminPermissions::default());
        assert!(
            unknown
                .members
                .iter()
                .all(|row| row.actions == MemberActions::default())
        );
        let levels = levels("11", json!({"@mod:example.org": 50}));
        let rows = vec![
            entry("@mod:example.org", AdminMembership::Join, "$a"),
            entry("@member:example.org", AdminMembership::Knock, "$b"),
        ];
        let known = assemble(own, Some(&levels), rows, "11");
        let own_row = known.member("@mod:example.org").unwrap();
        assert!(own_row.is_self && own_row.explicit_level);
        assert!(!own_row.actions.kick && !own_row.actions.ban && own_row.actions.change_power);
        let knock = known.member("@member:example.org").unwrap();
        assert!(knock.actions.approve && knock.actions.kick && !knock.actions.change_power);
        assert_eq!(known.counts.knocking, 1);
    }

    #[test]
    fn closed_member_list_and_account_aba_revoke_pending_work() {
        runtime().block_on(async {
            let selections = Selections::default();
            selections.replace(Some("casey-local"));
            let selection = selections.for_account("casey-local").unwrap();
            let cancelled = RoomSettingsCancellation::new();
            cancelled.cancel();
            let stages = AtomicUsize::new(0);
            let result = guarded(&selection, &cancelled, Duration::from_secs(1), async {
                stages.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await;
            assert_eq!(result, Err(RoomAdminError::Cancelled));
            assert_eq!(stages.load(Ordering::SeqCst), 0);

            let form = RoomSettingsCancellation::new();
            let (ready, wait) = tokio::sync::oneshot::channel::<()>();
            let mut operation =
                Box::pin(guarded(&selection, &form, Duration::from_secs(1), async {
                    wait.await.unwrap();
                    stages.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }));
            assert!(futures_util::poll!(operation.as_mut()).is_pending());
            selections.replace(Some("jules-local"));
            selections.replace(Some("casey-local"));
            ready.send(()).unwrap();
            assert_eq!(operation.await, Err(RoomAdminError::AccountChanged));
            assert_eq!(stages.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn write_failures_keep_only_fixed_categories() {
        use matrix_sdk::ruma::api::{
            client::uiaa::UiaaResponse,
            error::{Error, ErrorBody, FromHttpResponseError, StandardErrorBody},
        };
        for (status, wire_kind, expected) in [
            (429, "M_UNKNOWN", RoomAdminError::RateLimited),
            (400, "M_LIMIT_EXCEEDED", RoomAdminError::RateLimited),
            (403, "M_FORBIDDEN", RoomAdminError::Forbidden),
            (500, "M_UNKNOWN", RoomAdminError::Unavailable),
        ] {
            let body: StandardErrorBody = serde_json::from_value(json!({
                "errcode": wire_kind, "error": "private reflected response"
            }))
            .unwrap();
            let error = HttpError::Api(Box::new(FromHttpResponseError::Server(
                UiaaResponse::MatrixError(Error::new(
                    reqwest::StatusCode::from_u16(status).unwrap(),
                    ErrorBody::Standard(body),
                )),
            )));
            let public = write_failure(error);
            assert_eq!(public, expected);
            assert!(!format!("{public} {public:?}").contains("private"));
        }
    }

    #[test]
    fn inputs_are_bounded_and_power_writes_preserve_unknown_content() {
        assert!(parse_target("@name:example.org").is_ok());
        let long = format!("@{}:example.org", "a".repeat(255));
        for invalid in ["", "name", "@name", long.as_str()] {
            assert_eq!(parse_target(invalid), Err(RoomAdminError::InvalidUserId));
        }
        assert_eq!(normalize_reason(Some("  ")), Ok(None));
        assert_eq!(normalize_reason(Some(" spam ")), Ok(Some("spam".into())));
        assert!(normalize_reason(Some("a\u{7}b")).is_err());
        assert!(normalize_reason(Some(&"x".repeat(MAX_ADMIN_REASON_BYTES + 1))).is_err());

        let levels = levels("11", json!({"@mod:example.org": 50}));
        let current = envelope(
            "m.room.power_levels",
            "@casey:example.org",
            json!({"users": {"@mod:example.org": 50}, "org.example.policy": {"keep": true}}),
        );
        let target = user_id!("@member:example.org");
        let raw = member_level_content(&levels, Some(&current), target, Int::from(25u8)).unwrap();
        let written: Value = serde_json::from_str(raw.json().get()).unwrap();
        assert_eq!(written["users"]["@member:example.org"], json!(25));
        assert_eq!(written["users"]["@mod:example.org"], json!(50));
        assert_eq!(written["org.example.policy"], json!({"keep": true}));
    }

    #[test]
    fn thresholds_change_only_within_the_senders_own_level() {
        let own = user_id!("@mod:example.org");
        let levels = levels("11", json!({"@mod:example.org": 50}));
        let check = |current, level, reviewed| {
            permission_precondition(&levels, own, current, level, reviewed)
        };
        assert_eq!(check(50, 0, Some(MemberPower::Level(50))), Ok(()));
        assert_eq!(check(50, 0, None), Ok(()));
        assert_eq!(check(50, 60, None), Err(RoomAdminError::InsufficientPower));
        assert_eq!(check(90, 50, None), Err(RoomAdminError::InsufficientPower));
        assert_eq!(check(50, 50, None), Err(RoomAdminError::NotApplicable));
        let reviewed = Some(MemberPower::Level(0));
        assert_eq!(check(50, 0, reviewed), Err(RoomAdminError::TargetChanged));

        let rows = vec![entry("@mod:example.org", AdminMembership::Join, "$a")];
        let snapshot = assemble(own, Some(&levels), rows, "11");
        assert_eq!(snapshot.permissions.len(), PERMISSION_KEYS.len());
        let kick = snapshot.permission(PermissionKey::Kick).unwrap();
        assert!(kick.level == 50 && kick.editable);
        let locked = envelope(
            "m.room.power_levels",
            "@casey:example.org",
            json!({"users": {"@mod:example.org": 50}, "events": {"m.room.power_levels": 100}}),
        );
        let create = envelope(
            "m.room.create",
            "@casey:example.org",
            json!({"room_version": "11", "creator": "@casey:example.org"}),
        );
        let locked = room_settings::effective_power_levels(&create, Some(&locked))
            .unwrap()
            .unwrap();
        let rows = vec![entry("@mod:example.org", AdminMembership::Join, "$a")];
        let snapshot = assemble(own, Some(&locked), rows, "11");
        assert!(snapshot.permissions.iter().all(|row| !row.editable));
    }

    #[test]
    fn threshold_writes_edit_one_field_and_keep_unknown_content() {
        let levels = levels("11", json!({"@mod:example.org": 50}));
        let current = envelope(
            "m.room.power_levels",
            "@casey:example.org",
            json!({"kick": 50, "events": {"m.room.name": 50}, "org.example.policy": 1}),
        );
        let write = |key, level: u8| {
            let raw = permission_content(&levels, Some(&current), key, Int::from(level));
            serde_json::from_str::<Value>(raw.unwrap().json().get()).unwrap()
        };
        let written = write(PermissionKey::Kick, 0);
        assert_eq!(written["kick"], json!(0));
        assert_eq!(written["events"]["m.room.name"], json!(50));
        assert_eq!(written["org.example.policy"], json!(1));
        let written = write(PermissionKey::JoinRules, 100);
        assert_eq!(written["events"]["m.room.join_rules"], json!(100));
        assert_eq!(written["events"]["m.room.name"], json!(50));
        let written = write(PermissionKey::RoomNotification, 100);
        assert_eq!(written["notifications"]["room"], json!(100));
        assert_eq!(written["kick"], json!(50));
    }

    #[test]
    fn access_settings_edit_only_known_content_and_fence_the_reviewed_value() {
        let history = |content| {
            envelope("m.room.history_visibility", "@casey:example.org", content)
        };
        assert_eq!(history_choice(None), Some(HistoryChoice::Shared));
        let joined = history(json!({"history_visibility": "joined"}));
        assert_eq!(history_choice(Some(&joined)), Some(HistoryChoice::Joined));
        for unknown in [
            json!({"history_visibility": "joined", "org.example": 1}),
            json!({"history_visibility": "org.example.custom"}),
            json!({}),
        ] {
            assert_eq!(history_choice(Some(&history(unknown))), None);
        }
        assert_eq!(guests_can_join(None), Some(false));
        let guests = envelope(
            "m.room.guest_access",
            "@casey:example.org",
            json!({"guest_access": "can_join"}),
        );
        assert_eq!(guests_can_join(Some(&guests)), Some(true));

        let widen = AccessChange::History {
            from: HistoryChoice::Joined,
            to: HistoryChoice::WorldReadable,
        };
        assert!(access_matches(widen, Some(&joined)));
        assert!(!access_matches(widen, None));
        assert!(access_matches(AccessChange::Guests { allow: false }, Some(&guests)));
        assert!(!access_matches(AccessChange::Guests { allow: true }, Some(&guests)));
        let request = access_request(room_id!("!admin:example.org"), widen).unwrap();
        assert!(request.body.json().get().contains("world_readable"));

        let levels = levels("11", json!({"@mod:example.org": 50}));
        let own = user_id!("@mod:example.org");
        let access = access_settings(&levels, own, Some(&joined), None, false);
        assert!(access.history_editable && access.guests_editable);
        assert!(access.encryption_editable && !access.encrypted);
        assert_eq!(access.history_level, 50);
        let member = user_id!("@member:example.org");
        let locked = access_settings(&levels, member, Some(&joined), None, false);
        assert!(!locked.history_editable && !locked.guests_editable);
        assert!(!locked.encryption_editable);

        // Encryption only ever turns on, and only in a room without it.
        let on = access_settings(&levels, own, Some(&joined), None, true);
        assert!(on.encrypted && !on.encryption_editable);
        assert!(access_matches(AccessChange::Encrypt, None));
        let encryption = envelope(
            "m.room.encryption",
            "@casey:example.org",
            json!({"algorithm": "m.megolm.v1.aes-sha2"}),
        );
        assert!(!access_matches(AccessChange::Encrypt, Some(&encryption)));
        assert!(access_applied(AccessChange::Encrypt, on));
        let room = room_id!("!admin:example.org");
        let request = access_request(room, AccessChange::Encrypt).unwrap();
        assert!(request.body.json().get().contains("m.megolm.v1.aes-sha2"));
    }

    #[test]
    fn session_and_its_network_futures_keep_the_ui_worker_send_contract() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<RoomAdminSession>();
        send_sync::<RoomAdminSnapshot>();
        // Compile-time proof only; no client, account, device, or service is opened.
        #[allow(dead_code)]
        fn futures(
            session: &RoomAdminSession,
            manager: &AccountManager,
            selection: &AccountSelection,
            watch: &mut RosterWatch,
        ) {
            fn send<T: Send>(_: T) {}
            send(manager.open_room_admin_for(
                selection,
                "!admin:example.org",
                RoomSettingsCancellation::new(),
            ));
            send(session.refresh());
            send(session.apply(AdminRequest {
                action: AdminAction::Kick,
                user_id: "@member:example.org".into(),
                expected: MemberExpectation {
                    membership: Some(AdminMembership::Join),
                    power: Some(MemberPower::Level(0)),
                },
                reason: None,
            }));
            send(watch.changed());
        }
    }
}
