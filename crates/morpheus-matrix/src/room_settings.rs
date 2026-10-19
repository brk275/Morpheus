//! Revocable, exact-room settings sessions. Reads are authoritative server GETs;
//! per-field baseline checks detect observed changes, never promise atomic CAS.
//! Room-avatar state is shared metadata even when the room's messages are encrypted.

use std::{
    collections::BTreeSet,
    future::Future,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use matrix_sdk::{
    HttpError, Room, RoomState,
    config::RequestConfig,
    ruma::{
        OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, RoomVersionId, UInt,
        api::{
            client::state::{get_state_event_for_key, send_state_event},
            error::ErrorKind,
        },
        events::{
            EmptyStateKey, StateEventContent, StateEventType,
            room::{
                MediaSource,
                avatar::{ImageInfo, RoomAvatarEventContent},
                create::RoomCreateEventContent,
                join_rules::{AllowRule, JoinRule, Restricted, RoomJoinRulesEventContent},
                member::{MembershipState, RoomMemberEventContent},
                name::RoomNameEventContent,
                power_levels::{
                    RoomPowerLevels, RoomPowerLevelsEventContent, RoomPowerLevelsSource,
                },
                topic::RoomTopicEventContent,
            },
        },
    },
};
use serde::Deserialize;
use serde_json::Value;
use tokio::{
    sync::{Mutex, Notify},
    time,
};

use crate::{AccountManager, AccountSelection, account_room};

pub const MAX_ROOM_AVATAR_UPLOAD_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_ROOM_AVATAR_SIDE: u32 = 2048;
pub const MAX_ROOM_AVATAR_PIXELS: u64 = 4 * 1024 * 1024;
pub const MAX_ROOM_AVATAR_PREVIEW_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RESTRICTED_ALLOW_ROOMS: usize = 32;
const MAX_STATE_BYTES: usize = 64 * 1024;
const MAX_ROOM_ID_BYTES: usize = 1024;
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(45);
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RoomSettingsError {
    #[error("This room settings form is closed")]
    Cancelled,
    #[error("The selected account changed")]
    AccountChanged,
    #[error("Join this room before changing its settings")]
    NotJoined,
    #[error("Room settings are unavailable; refresh before retrying")]
    Unavailable,
    #[error("The homeserver is limiting requests; wait and refresh before retrying")]
    RateLimited,
    #[error("The operation timed out; refresh before retrying")]
    TimedOut,
    #[error("The server returned unsupported or invalid room state")]
    InvalidState,
    #[error("Your current room permissions do not allow this change")]
    PermissionDenied,
    #[error("This room access rule is read-only in Morpheus")]
    ReadOnly,
    #[error("This setting changed on the server; refresh before saving")]
    BaselineChanged,
    #[error("Choose a valid bounded PNG avatar")]
    InvalidAvatar,
    #[error("Choose a supported room access rule and valid allowed room IDs")]
    InvalidJoinRule,
    #[error("Enter a nonempty room name of at most 1024 bytes or a topic of at most 16 KiB")]
    InvalidText,
    #[error("Image previews in encrypted rooms are disabled")]
    EncryptedPreviewDisabled,
}
type Result<T> = std::result::Result<T, RoomSettingsError>;

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}
/// Create before opening the form. Cancelling any clone permanently revokes
/// every session and pending operation using this token. It does not undo a
/// request already handed to the homeserver.
#[derive(Clone, Default)]
pub struct RoomSettingsCancellation(Arc<CancellationState>);
impl RoomSettingsCancellation {
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
    pub(crate) async fn cancelled(&self) {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinRuleKind {
    Invite,
    Public,
    Knock,
    Restricted,
    KnockRestricted,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditableJoinRule {
    Invite,
    Public,
    Knock,
    Restricted { allow_rooms: Vec<String> },
    KnockRestricted { allow_rooms: Vec<String> },
}
impl EditableJoinRule {
    pub fn kind(&self) -> JoinRuleKind {
        match self {
            Self::Invite => JoinRuleKind::Invite,
            Self::Public => JoinRuleKind::Public,
            Self::Knock => JoinRuleKind::Knock,
            Self::Restricted { .. } => JoinRuleKind::Restricted,
            Self::KnockRestricted { .. } => JoinRuleKind::KnockRestricted,
        }
    }
    fn wire(&self) -> Result<JoinRule> {
        Ok(match self {
            Self::Invite => JoinRule::Invite,
            Self::Public => JoinRule::Public,
            Self::Knock => JoinRule::Knock,
            Self::Restricted { allow_rooms } => {
                JoinRule::Restricted(Restricted::new(allowed_rooms(allow_rooms)?))
            }
            Self::KnockRestricted { allow_rooms } => {
                JoinRule::KnockRestricted(Restricted::new(allowed_rooms(allow_rooms)?))
            }
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOnlyJoinRule {
    Missing,
    Private,
    Unknown,
    UnsupportedRoomVersion,
    UnsupportedAllowRules,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CurrentJoinRule {
    Editable(EditableJoinRule),
    ReadOnly(ReadOnlyJoinRule),
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoomSettingsPermissions {
    pub name: bool,
    pub topic: bool,
    pub avatar: bool,
    pub join_rule: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomAvatarSummary {
    pub present: bool,
    pub preview_available: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomSettingsSnapshot {
    pub name: String,
    pub topic: String,
    pub avatar: RoomAvatarSummary,
    pub join_rule: CurrentJoinRule,
    pub available_join_rules: Vec<JoinRuleKind>,
    pub permissions: RoomSettingsPermissions,
    pub room_version: String,
    pub encrypted: bool,
}

/// No Debug/Serialize: image bytes remain private, in memory. The caller must
/// decode then encode stripped RGB/RGBA pixels before constructing this DTO.
pub struct AvatarPng {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
}
impl AvatarPng {
    pub fn new(bytes: Vec<u8>, width: u32, height: u32) -> Result<Self> {
        validate_png(&bytes, width, height)?;
        Ok(Self {
            bytes,
            width,
            height,
        })
    }
}
/// Session-resolved avatar input, never an arbitrary caller URL. Decode using
/// the UI's bounded image worker and discard when the form is revoked.
pub struct RoomAvatarPreview {
    pub bytes: Vec<u8>,
}

#[derive(Clone)]
struct Loaded {
    snapshot: RoomSettingsSnapshot,
    name: Option<Value>,
    topic: Option<Value>,
    avatar: Option<Value>,
    join_rule: Option<Value>,
    avatar_source: Option<MediaSource>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsField {
    Name,
    Topic,
    Avatar,
    JoinRule,
}

// Saving one field must not silently accept a remote change to another field
// while the UI may still contain an unsaved edit based on its original value.
// Explicit Refresh replaces all baselines. A write advances only its own;
// current permissions, room version and encryption state remain fresh.
fn preserve_other_fields(previous: &Loaded, mut fresh: Loaded, saved: SettingsField) -> Loaded {
    if saved != SettingsField::Name {
        fresh.name = previous.name.clone();
        fresh.snapshot.name = previous.snapshot.name.clone();
    }
    if saved != SettingsField::Topic {
        fresh.topic = previous.topic.clone();
        fresh.snapshot.topic = previous.snapshot.topic.clone();
    }
    if saved != SettingsField::Avatar {
        fresh.avatar = previous.avatar.clone();
        fresh.avatar_source = previous.avatar_source.clone();
        fresh.snapshot.avatar = previous.snapshot.avatar.clone();
    }
    if saved != SettingsField::JoinRule {
        fresh.join_rule = previous.join_rule.clone();
        fresh.snapshot.join_rule = previous.snapshot.join_rule.clone();
    }
    fresh
}

pub struct RoomSettingsSession {
    selection: AccountSelection,
    cancellation: RoomSettingsCancellation,
    room: Room,
    own_user: OwnedUserId,
    loaded: RwLock<Loaded>,
    operation: Mutex<()>,
}

impl AccountManager {
    pub async fn open_room_settings_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        cancellation: RoomSettingsCancellation,
    ) -> Result<RoomSettingsSession> {
        ensure_scope(selection, &cancellation)?;
        let (room, own_user) = guarded(selection, &cancellation, READ_TIMEOUT, async {
            let state = self.state.lock().await;
            let current = self
                .account_selection(selection.account_id())
                .ok_or(RoomSettingsError::AccountChanged)?;
            if !selection.same_epoch(&current) {
                return Err(RoomSettingsError::AccountChanged);
            }
            ensure_scope(selection, &cancellation)?;
            let room = account_room(&state, selection.account_id(), room_id)
                .map_err(|_| RoomSettingsError::Unavailable)?;
            ensure_joined(&room)?;
            let own_user = room
                .client()
                .user_id()
                .ok_or(RoomSettingsError::Unavailable)?
                .to_owned();
            Ok((room, own_user))
        })
        .await?;
        let loaded = guarded(
            selection,
            &cancellation,
            READ_TIMEOUT,
            read_settings(&room, &own_user),
        )
        .await?;
        ensure_scope(selection, &cancellation)?;
        Ok(RoomSettingsSession {
            selection: selection.clone(),
            cancellation,
            room,
            own_user,
            loaded: RwLock::new(loaded),
            operation: Mutex::new(()),
        })
    }
}

impl RoomSettingsSession {
    pub fn account_id(&self) -> &str {
        self.selection.account_id()
    }
    pub fn room_id(&self) -> &str {
        self.room.room_id().as_str()
    }
    pub fn snapshot(&self) -> RoomSettingsSnapshot {
        self.loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot
            .clone()
    }
    fn check(&self) -> Result<()> {
        ensure_scope(&self.selection, &self.cancellation)?;
        ensure_joined(&self.room)
    }
    async fn run<T>(&self, limit: Duration, work: impl Future<Output = Result<T>>) -> Result<T> {
        guarded(&self.selection, &self.cancellation, limit, async {
            let _operation = self.operation.lock().await;
            self.check()?;
            work.await
        })
        .await
    }
    fn publish(&self, fresh: Loaded) -> Result<RoomSettingsSnapshot> {
        self.check()?;
        let snapshot = fresh.snapshot.clone();
        *self.loaded.write().unwrap_or_else(|e| e.into_inner()) = fresh;
        Ok(snapshot)
    }
    fn publish_write(&self, fresh: Loaded, saved: SettingsField) -> Result<RoomSettingsSnapshot> {
        self.check()?;
        let mut loaded = self.loaded.write().unwrap_or_else(|e| e.into_inner());
        let fresh = preserve_other_fields(&loaded, fresh, saved);
        let snapshot = fresh.snapshot.clone();
        *loaded = fresh;
        Ok(snapshot)
    }
    pub async fn refresh(&self) -> Result<RoomSettingsSnapshot> {
        self.run(READ_TIMEOUT, async {
            self.publish(read_settings(&self.room, &self.own_user).await?)
        })
        .await
    }
    pub async fn set_name(&self, name: &str) -> Result<RoomSettingsSnapshot> {
        let name = editable_text(name, true)?;
        self.run(WRITE_TIMEOUT, async {
            let baseline = self
                .loaded
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .name
                .clone();
            let fresh = read_settings(&self.room, &self.own_user).await?;
            require_baseline(&baseline, &fresh.name)?;
            if !fresh.snapshot.permissions.name {
                return Err(RoomSettingsError::PermissionDenied);
            }
            self.check()?;
            send_state(&self.room, RoomNameEventContent::new(name.to_owned())).await?;
            self.check()?;
            self.publish_write(
                read_settings(&self.room, &self.own_user).await?,
                SettingsField::Name,
            )
        })
        .await
    }
    pub async fn set_topic(&self, topic: &str) -> Result<RoomSettingsSnapshot> {
        let topic = editable_text(topic, false)?;
        self.run(WRITE_TIMEOUT, async {
            let baseline = self
                .loaded
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .topic
                .clone();
            let fresh = read_settings(&self.room, &self.own_user).await?;
            require_baseline(&baseline, &fresh.topic)?;
            if !fresh.snapshot.permissions.topic {
                return Err(RoomSettingsError::PermissionDenied);
            }
            self.check()?;
            send_state(&self.room, RoomTopicEventContent::new(topic.to_owned())).await?;
            self.check()?;
            self.publish_write(
                read_settings(&self.room, &self.own_user).await?,
                SettingsField::Topic,
            )
        })
        .await
    }
    pub async fn set_join_rule(&self, rule: EditableJoinRule) -> Result<RoomSettingsSnapshot> {
        let wire = rule.wire()?;
        self.run(WRITE_TIMEOUT, async {
            let baseline = self
                .loaded
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .join_rule
                .clone();
            let fresh = read_settings(&self.room, &self.own_user).await?;
            require_baseline(&baseline, &fresh.join_rule)?;
            if !matches!(fresh.snapshot.join_rule, CurrentJoinRule::Editable(_)) {
                return Err(RoomSettingsError::ReadOnly);
            }
            if !fresh.snapshot.available_join_rules.contains(&rule.kind()) {
                return Err(RoomSettingsError::InvalidJoinRule);
            }
            if !fresh.snapshot.permissions.join_rule {
                return Err(RoomSettingsError::PermissionDenied);
            }
            self.check()?;
            send_state(&self.room, RoomJoinRulesEventContent::new(wire)).await?;
            self.check()?;
            self.publish_write(
                read_settings(&self.room, &self.own_user).await?,
                SettingsField::JoinRule,
            )
        })
        .await
    }
    pub async fn set_avatar(&self, image: AvatarPng) -> Result<RoomSettingsSnapshot> {
        self.run(WRITE_TIMEOUT, async {
            let baseline = self
                .loaded
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .avatar
                .clone();
            self.avatar_permission(&baseline).await?;
            self.check()?;
            // Deliberately split upload and state write. A revoked form can
            // leave an unused upload, but never starts the later state change.
            let size = image.bytes.len();
            let upload = self
                .room
                .client()
                .media()
                .upload(
                    &mime::IMAGE_PNG,
                    image.bytes,
                    Some(RequestConfig::new().retry_limit(0).timeout(UPLOAD_TIMEOUT)),
                )
                .await
                .map_err(|_| RoomSettingsError::Unavailable)?;
            self.check()?;
            crate::media::validate_media_address(&upload.content_uri)
                .map_err(|_| RoomSettingsError::InvalidState)?;
            self.avatar_permission(&baseline).await?;
            let mut info = ImageInfo::new();
            info.width = Some(UInt::from(image.width));
            info.height = Some(UInt::from(image.height));
            info.size = Some(UInt::try_from(size).map_err(|_| RoomSettingsError::InvalidAvatar)?);
            info.mimetype = Some("image/png".into());
            self.check()?;
            let mut avatar = RoomAvatarEventContent::new();
            avatar.url = Some(upload.content_uri);
            avatar.info = Some(Box::new(info));
            send_state(&self.room, avatar).await?;
            self.check()?;
            self.publish_write(
                read_settings(&self.room, &self.own_user).await?,
                SettingsField::Avatar,
            )
        })
        .await
    }
    async fn avatar_permission(&self, baseline: &Option<Value>) -> Result<()> {
        let fresh = read_settings(&self.room, &self.own_user).await?;
        self.check()?;
        require_baseline(baseline, &fresh.avatar)?;
        if !fresh.snapshot.permissions.avatar {
            return Err(RoomSettingsError::PermissionDenied);
        }
        Ok(())
    }
    pub async fn remove_avatar(&self) -> Result<RoomSettingsSnapshot> {
        self.run(WRITE_TIMEOUT, async {
            let baseline = self
                .loaded
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .avatar
                .clone();
            self.avatar_permission(&baseline).await?;
            self.check()?;
            send_state(&self.room, RoomAvatarEventContent::new()).await?;
            self.check()?;
            self.publish_write(
                read_settings(&self.room, &self.own_user).await?,
                SettingsField::Avatar,
            )
        })
        .await
    }
    pub async fn download_avatar_preview(
        &self,
        allow_encrypted: bool,
    ) -> Result<Option<RoomAvatarPreview>> {
        self.run(READ_TIMEOUT, async {
            let fresh = read_settings(&self.room, &self.own_user).await?;
            self.check()?;
            require_preview_policy(fresh.snapshot.encrypted, allow_encrypted)?;
            let Some(source) = fresh.avatar_source else {
                return Ok(None);
            };
            let bytes = crate::media::fetch_bounded_media(
                &self.room.client(),
                &source,
                MAX_ROOM_AVATAR_PREVIEW_BYTES,
            )
            .await
            .map_err(|_| RoomSettingsError::Unavailable)?;
            self.check()?;
            // Do not publish/replace the editable baseline as a side effect of
            // reading a preview. An avatar change still needs explicit refresh.
            Ok(Some(RoomAvatarPreview { bytes }))
        })
        .await
    }
}

pub(crate) fn ensure_scope(
    selection: &AccountSelection,
    cancellation: &RoomSettingsCancellation,
) -> Result<()> {
    if cancellation.is_cancelled() {
        return Err(RoomSettingsError::Cancelled);
    }
    if !selection.is_current() {
        return Err(RoomSettingsError::AccountChanged);
    }
    Ok(())
}
pub(crate) fn ensure_joined(room: &Room) -> Result<()> {
    if room.state() != RoomState::Joined {
        return Err(RoomSettingsError::NotJoined);
    }
    Ok(())
}
async fn guarded<T>(
    selection: &AccountSelection,
    cancellation: &RoomSettingsCancellation,
    limit: Duration,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    ensure_scope(selection, cancellation)?;
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(RoomSettingsError::Cancelled),
        _ = selection.revoked() => Err(RoomSettingsError::AccountChanged),
        result = time::timeout(limit, work) => result.map_err(|_| RoomSettingsError::TimedOut)?,
    }?;
    ensure_scope(selection, cancellation)?;
    Ok(result)
}
fn require_preview_policy(encrypted: bool, allow_encrypted: bool) -> Result<()> {
    if encrypted && !allow_encrypted {
        return Err(RoomSettingsError::EncryptedPreviewDisabled);
    }
    Ok(())
}
fn editable_text(value: &str, name: bool) -> Result<&str> {
    let value = if name { value.trim() } else { value };
    let maximum = if name { 1024 } else { 16 * 1024 };
    if value.len() > maximum || (name && value.is_empty()) {
        return Err(RoomSettingsError::InvalidText);
    }
    Ok(value)
}
fn require_baseline(expected: &Option<Value>, current: &Option<Value>) -> Result<()> {
    if expected != current {
        return Err(RoomSettingsError::BaselineChanged);
    }
    Ok(())
}

#[derive(Clone, Deserialize)]
pub(crate) struct StateEnvelope {
    pub(crate) event_id: OwnedEventId,
    room_id: OwnedRoomId,
    sender: OwnedUserId,
    #[serde(rename = "type")]
    event_type: StateEventType,
    state_key: String,
    pub(crate) content: Value,
}
fn parse_state(
    raw: &str,
    room: &RoomId,
    kind: &StateEventType,
    key: &str,
) -> Result<StateEnvelope> {
    if raw.len() > MAX_STATE_BYTES {
        return Err(RoomSettingsError::InvalidState);
    }
    let value: Value = serde_json::from_str(raw).map_err(|_| RoomSettingsError::InvalidState)?;
    if !value.is_object() {
        return Err(RoomSettingsError::InvalidState);
    }
    let event: StateEnvelope =
        serde_json::from_value(value).map_err(|_| RoomSettingsError::InvalidState)?;
    if event.room_id.as_str() != room.as_str()
        || &event.event_type != kind
        || event.state_key != key
        || !event.content.is_object()
    {
        return Err(RoomSettingsError::InvalidState);
    }
    Ok(event)
}
fn missing(error: &HttpError) -> bool {
    error.as_client_api_error().is_some_and(|e| {
        e.status_code.as_u16() == 404 && e.error_kind() == Some(&ErrorKind::NotFound)
    })
}
// Keep only a fixed category. Never retain reflected response bodies, URLs or
// server-provided Retry-After text, and never retry a state write automatically.
fn request_failure(error: HttpError) -> RoomSettingsError {
    if error.as_client_api_error().is_some_and(|error| {
        error.status_code.as_u16() == 429
            || matches!(error.error_kind(), Some(ErrorKind::LimitExceeded(_)))
    }) {
        RoomSettingsError::RateLimited
    } else {
        RoomSettingsError::Unavailable
    }
}
pub(crate) async fn state(
    room: &Room,
    kind: StateEventType,
    key: &str,
) -> Result<Option<StateEnvelope>> {
    let mut request = get_state_event_for_key::v3::Request::new(
        room.room_id().to_owned(),
        kind.clone(),
        key.into(),
    );
    request.format = get_state_event_for_key::v3::StateEventFormat::Event;
    match room
        .client()
        .send(request)
        .with_request_config(RequestConfig::new().retry_limit(0).timeout(REQUEST_TIMEOUT))
        .await
    {
        Ok(response) => {
            parse_state(response.event_or_content.get(), room.room_id(), &kind, key).map(Some)
        }
        Err(error) if missing(&error) => Ok(None),
        Err(error) => Err(request_failure(error)),
    }
}
// The pinned client has encrypted-state events disabled; these standard room
// settings are shared state. Typed direct requests let us bound every write
// independently and avoid transparent retries of a changed baseline.
async fn send_state(
    room: &Room,
    content: impl StateEventContent<StateKey = EmptyStateKey>,
) -> Result<()> {
    ensure_joined(room)?;
    let request =
        send_state_event::v3::Request::new(room.room_id().to_owned(), &EmptyStateKey, &content)
            .map_err(|_| RoomSettingsError::InvalidState)?;
    room.client()
        .send(request)
        .with_request_config(RequestConfig::new().retry_limit(0).timeout(REQUEST_TIMEOUT))
        .await
        .map_err(request_failure)?;
    Ok(())
}
pub(crate) fn content<T: serde::de::DeserializeOwned>(event: &StateEnvelope) -> Result<T> {
    serde_json::from_value(event.content.clone()).map_err(|_| RoomSettingsError::InvalidState)
}
async fn read_settings(room: &Room, own: &OwnedUserId) -> Result<Loaded> {
    ensure_joined(room)?;
    let create = state(room, StateEventType::RoomCreate, "")
        .await?
        .ok_or(RoomSettingsError::InvalidState)?;
    let member = state(room, StateEventType::RoomMember, own.as_str())
        .await?
        .ok_or(RoomSettingsError::NotJoined)?;
    let member: RoomMemberEventContent = content(&member)?;
    if member.membership != MembershipState::Join {
        return Err(RoomSettingsError::NotJoined);
    }
    let power = state(room, StateEventType::RoomPowerLevels, "").await?;
    let avatar = state(room, StateEventType::RoomAvatar, "").await?;
    let join = state(room, StateEventType::RoomJoinRules, "").await?;
    let name = state(room, StateEventType::RoomName, "").await?;
    let topic = state(room, StateEventType::RoomTopic, "").await?;
    let encryption = state(room, StateEventType::RoomEncryption, "").await?;
    ensure_joined(room)?;
    assemble(
        own,
        &create,
        power.as_ref(),
        avatar.as_ref(),
        join.as_ref(),
        name.as_ref(),
        topic.as_ref(),
        encryption.is_some(),
    )
}

#[allow(clippy::too_many_arguments)]
fn assemble(
    own: &OwnedUserId,
    create: &StateEnvelope,
    power: Option<&StateEnvelope>,
    avatar: Option<&StateEnvelope>,
    join: Option<&StateEnvelope>,
    name: Option<&StateEnvelope>,
    topic: Option<&StateEnvelope>,
    encrypted: bool,
) -> Result<Loaded> {
    let creation: RoomCreateEventContent = content(create)?;
    let mut permissions = RoomSettingsPermissions::default();
    let available = rule_options(&creation.room_version);
    if let Some(levels) = effective_power_levels(create, power)? {
        permissions = RoomSettingsPermissions {
            name: levels.user_can_send_state(own, StateEventType::RoomName),
            topic: levels.user_can_send_state(own, StateEventType::RoomTopic),
            avatar: levels.user_can_send_state(own, StateEventType::RoomAvatar),
            join_rule: levels.user_can_send_state(own, StateEventType::RoomJoinRules),
        };
    }
    let join_rule = current_rule(join.map(|e| &e.content), &creation.room_version);
    if matches!(join_rule, CurrentJoinRule::ReadOnly(_)) {
        permissions.join_rule = false;
    }
    let avatar_content = avatar.map(content::<RoomAvatarEventContent>).transpose()?;
    let url = avatar_content.as_ref().and_then(|a| a.url.as_ref());
    let avatar_source = url
        .filter(|url| crate::media::validate_media_address(url).is_ok())
        .cloned()
        .map(MediaSource::Plain);
    let info = avatar_content.as_ref().and_then(|a| a.info.as_ref());
    let summary = RoomAvatarSummary {
        present: url.is_some(),
        preview_available: avatar_source.is_some(),
        width: info
            .and_then(|i| i.width)
            .and_then(|x| u32::try_from(u64::from(x)).ok()),
        height: info
            .and_then(|i| i.height)
            .and_then(|x| u32::try_from(u64::from(x)).ok()),
    };
    Ok(Loaded {
        snapshot: RoomSettingsSnapshot {
            name: state_text(name, "name", 1024)?,
            topic: state_text(topic, "topic", 16 * 1024)?,
            avatar: summary,
            join_rule,
            available_join_rules: available,
            permissions,
            room_version: creation.room_version.as_str().to_owned(),
            encrypted,
        },
        avatar: avatar.map(|e| e.content.clone()),
        name: name.map(|e| e.content.clone()),
        topic: topic.map(|e| e.content.clone()),
        join_rule: join.map(|e| e.content.clone()),
        avatar_source,
    })
}
/// Effective levels from authoritative create and power-level state. `None`
/// for a room version whose authorization rules are unknown to the pinned
/// client; callers must then deny every privileged action.
pub(crate) fn effective_power_levels(
    create: &StateEnvelope,
    power: Option<&StateEnvelope>,
) -> Result<Option<RoomPowerLevels>> {
    let creation: RoomCreateEventContent = content(create)?;
    let Some(rules) = creation.room_version.rules() else {
        return Ok(None);
    };
    let mut creators = if rules.authorization.use_room_create_sender {
        vec![create.sender.clone()]
    } else {
        #[allow(deprecated)]
        vec![
            creation
                .creator
                .clone()
                .ok_or(RoomSettingsError::InvalidState)?,
        ]
    };
    if rules.authorization.additional_room_creators {
        creators.extend(creation.additional_creators.clone());
    }
    if creators.len() > 64 {
        return Err(RoomSettingsError::InvalidState);
    }
    let power = match power {
        Some(event) => {
            RoomPowerLevelsSource::Original(content::<RoomPowerLevelsEventContent>(event)?)
        }
        None => RoomPowerLevelsSource::None,
    };
    Ok(Some(RoomPowerLevels::new(power, &rules.authorization, creators)))
}
fn state_text(event: Option<&StateEnvelope>, key: &str, maximum: usize) -> Result<String> {
    match event.and_then(|e| e.content.get(key)) {
        None => Ok(String::new()),
        Some(Value::String(s)) if s.len() <= maximum => Ok(s.clone()),
        _ => Err(RoomSettingsError::InvalidState),
    }
}
fn rule_options(version: &RoomVersionId) -> Vec<JoinRuleKind> {
    let Some(rules) = version.rules() else {
        return Vec::new();
    };
    let mut result = vec![JoinRuleKind::Invite, JoinRuleKind::Public];
    if rules.authorization.knocking {
        result.push(JoinRuleKind::Knock);
    }
    if rules.authorization.restricted_join_rule {
        result.push(JoinRuleKind::Restricted);
    }
    if rules.authorization.knock_restricted_join_rule {
        result.push(JoinRuleKind::KnockRestricted);
    }
    result
}
fn allowed_rooms(rooms: &[String]) -> Result<Vec<AllowRule>> {
    if rooms.len() > MAX_RESTRICTED_ALLOW_ROOMS {
        return Err(RoomSettingsError::InvalidJoinRule);
    }
    let mut seen = BTreeSet::new();
    rooms
        .iter()
        .map(|id| {
            if id.len() > MAX_ROOM_ID_BYTES || !seen.insert(id) {
                return Err(RoomSettingsError::InvalidJoinRule);
            }
            let room = RoomId::parse(id).map_err(|_| RoomSettingsError::InvalidJoinRule)?;
            Ok(AllowRule::room_membership(room))
        })
        .collect()
}
fn current_rule(raw: Option<&Value>, version: &RoomVersionId) -> CurrentJoinRule {
    use ReadOnlyJoinRule as ReadOnly;
    let readonly = |reason| CurrentJoinRule::ReadOnly(reason);
    if version.rules().is_none() {
        return readonly(ReadOnly::UnsupportedRoomVersion);
    }
    let Some(raw) = raw else {
        return readonly(ReadOnly::Missing);
    };
    let Some(object) = raw.as_object() else {
        return readonly(ReadOnly::Unknown);
    };
    let Some(kind) = object.get("join_rule").and_then(Value::as_str) else {
        return readonly(ReadOnly::Unknown);
    };
    if object
        .keys()
        .any(|key| key != "join_rule" && key != "allow")
    {
        return readonly(ReadOnly::Unknown);
    }
    let rule = match kind {
        "invite" | "public" | "knock" if !object.contains_key("allow") => match kind {
            "invite" => EditableJoinRule::Invite,
            "public" => EditableJoinRule::Public,
            _ => EditableJoinRule::Knock,
        },
        "private" => return readonly(ReadOnly::Private),
        "restricted" | "knock_restricted" => {
            // Ruma intentionally ignores invalid allow entries. A settings
            // editor must not silently erase those entries on save instead.
            let entries: &[Value] = match object.get("allow") {
                None => &[], // The protocol defaults a missing allow list to empty.
                Some(Value::Array(entries)) => entries,
                _ => return readonly(ReadOnly::UnsupportedAllowRules),
            };
            if entries.len() > MAX_RESTRICTED_ALLOW_ROOMS {
                return readonly(ReadOnly::UnsupportedAllowRules);
            }
            let mut rooms = Vec::with_capacity(entries.len());
            for entry in entries {
                let Some(entry) = entry.as_object() else {
                    return readonly(ReadOnly::UnsupportedAllowRules);
                };
                if entry.len() != 2
                    || entry.get("type").and_then(Value::as_str) != Some("m.room_membership")
                {
                    return readonly(ReadOnly::UnsupportedAllowRules);
                }
                let Some(room) = entry.get("room_id").and_then(Value::as_str) else {
                    return readonly(ReadOnly::UnsupportedAllowRules);
                };
                rooms.push(room.to_owned());
            }
            if allowed_rooms(&rooms).is_err() {
                return readonly(ReadOnly::UnsupportedAllowRules);
            }
            if kind == "restricted" {
                EditableJoinRule::Restricted { allow_rooms: rooms }
            } else {
                EditableJoinRule::KnockRestricted { allow_rooms: rooms }
            }
        }
        _ => return readonly(ReadOnly::Unknown),
    };
    if !rule_options(version).contains(&rule.kind()) {
        return readonly(ReadOnly::UnsupportedRoomVersion);
    }
    CurrentJoinRule::Editable(rule)
}

fn validate_png(bytes: &[u8], width: u32, height: u32) -> Result<()> {
    let bad = RoomSettingsError::InvalidAvatar;
    if bytes.len() > MAX_ROOM_AVATAR_UPLOAD_BYTES
        || width == 0
        || height == 0
        || width > MAX_ROOM_AVATAR_SIDE
        || height > MAX_ROOM_AVATAR_SIDE
        || u64::from(width) * u64::from(height) > MAX_ROOM_AVATAR_PIXELS
        || !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
    {
        return Err(bad);
    }
    let mut offset = 8usize;
    let mut chunks = 0usize;
    let mut data = false;
    while offset < bytes.len() {
        chunks += 1;
        if chunks > 1024 || bytes.len() - offset < 12 {
            return Err(bad);
        }
        let length =
            u32::from_be_bytes(bytes[offset..offset + 4].try_into().map_err(|_| bad)?) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|n| n.checked_add(length))
            .filter(|n| *n <= bytes.len())
            .ok_or(bad)?;
        let kind = &bytes[offset + 4..offset + 8];
        let body = &bytes[offset + 8..end - 4];
        let expected = u32::from_be_bytes(bytes[end - 4..end].try_into().map_err(|_| bad)?);
        if crc32(&bytes[offset + 4..end - 4]) != expected {
            return Err(bad);
        }
        match kind {
            b"IHDR" if chunks == 1 && body.len() == 13 => {
                if u32::from_be_bytes(body[..4].try_into().map_err(|_| bad)?) != width
                    || u32::from_be_bytes(body[4..8].try_into().map_err(|_| bad)?) != height
                    || body[8] != 8
                    || !matches!(body[9], 2 | 6)
                    || body[10..] != [0, 0, 0]
                {
                    return Err(bad);
                }
            }
            b"IDAT" if chunks > 1 && !body.is_empty() => data = true,
            b"IEND" if chunks > 1 && data && body.is_empty() && end == bytes.len() => return Ok(()),
            _ => return Err(bad),
        }
        offset = end;
    }
    Err(bad)
}
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ ((0u32.wrapping_sub(crc & 1)) & 0xedb8_8320);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_selection::Selections;
    use matrix_sdk::ruma::{room_id, user_id};
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn request_errors_keep_only_fixed_rate_limit_or_unavailable_categories() {
        use matrix_sdk::ruma::api::{
            client::uiaa::UiaaResponse,
            error::{Error, ErrorBody, FromHttpResponseError, StandardErrorBody},
        };
        for (status, wire_kind, expected) in [
            (429, "M_UNKNOWN", RoomSettingsError::RateLimited),
            (400, "M_LIMIT_EXCEEDED", RoomSettingsError::RateLimited),
            (403, "M_FORBIDDEN", RoomSettingsError::Unavailable),
            (500, "M_UNKNOWN", RoomSettingsError::Unavailable),
        ] {
            let body: StandardErrorBody = serde_json::from_value(json!({
                "errcode": wire_kind, "error": "private reflected response",
                "retry_after_ms": 60000
            }))
            .unwrap();
            let error = HttpError::Api(Box::new(FromHttpResponseError::Server(
                UiaaResponse::MatrixError(Error::new(
                    reqwest::StatusCode::from_u16(status).unwrap(),
                    ErrorBody::Standard(body),
                )),
            )));
            let public = request_failure(error);
            assert_eq!(public, expected);
            assert!(!public.to_string().contains("private"));
            assert!(!format!("{public:?}").contains("60000"));
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }
    fn envelope(kind: StateEventType, body: Value) -> StateEnvelope {
        parse_state(
            &json!({
                "event_id":"$fixture", "room_id":"!settings:example.org",
                "sender":"@casey:example.org", "type":kind, "state_key":"", "content":body
            })
            .to_string(),
            room_id!("!settings:example.org"),
            &kind,
            "",
        )
        .unwrap()
    }
    fn creation(version: &str) -> StateEnvelope {
        envelope(
            StateEventType::RoomCreate,
            json!({
                "room_version":version,"creator":"@casey:example.org"
            }),
        )
    }
    fn loaded(
        create: &StateEnvelope,
        power: Option<&StateEnvelope>,
        join: Option<&StateEnvelope>,
    ) -> Loaded {
        assemble(
            &user_id!("@jules:example.org").to_owned(),
            create,
            power,
            None,
            join,
            None,
            None,
            false,
        )
        .unwrap()
    }

    #[test]
    fn form_cancellation_is_permanent_and_wakes_all_registered_clones() {
        runtime().block_on(async {
            let cancellation = RoomSettingsCancellation::new();
            let copies: Vec<_> = (0..4).map(|_| cancellation.clone()).collect();
            let mut waits: Vec<_> = copies
                .iter()
                .map(|copy| Box::pin(copy.cancelled()))
                .collect();
            for wait in &mut waits {
                assert!(futures_util::poll!(wait.as_mut()).is_pending());
            }
            cancellation.cancel();
            for wait in waits {
                time::timeout(Duration::from_secs(1), wait).await.unwrap();
            }
            time::timeout(Duration::from_secs(1), cancellation.clone().cancelled())
                .await
                .unwrap();
        });
    }

    #[test]
    fn closed_form_does_not_poll_work_and_cancelled_upload_cannot_start_next_step() {
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
            assert_eq!(result, Err(RoomSettingsError::Cancelled));
            assert_eq!(stages.load(Ordering::SeqCst), 0);

            let form = RoomSettingsCancellation::new();
            let (finish_upload, uploaded) = tokio::sync::oneshot::channel::<()>();
            let mut operation =
                Box::pin(guarded(&selection, &form, Duration::from_secs(1), async {
                    stages.fetch_add(1, Ordering::SeqCst);
                    uploaded.await.unwrap();
                    ensure_scope(&selection, &form)?;
                    stages.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }));
            assert!(futures_util::poll!(operation.as_mut()).is_pending());
            form.cancel();
            finish_upload.send(()).unwrap();
            assert_eq!(operation.await, Err(RoomSettingsError::Cancelled));
            assert_eq!(stages.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn account_aba_revokes_pending_work_even_when_form_stays_open() {
        runtime().block_on(async {
            let selections = Selections::default();
            selections.replace(Some("casey-local"));
            let selection = selections.for_account("casey-local").unwrap();
            let form = RoomSettingsCancellation::new();
            let (ready, wait) = tokio::sync::oneshot::channel::<()>();
            let stages = AtomicUsize::new(0);
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
            assert_eq!(operation.await, Err(RoomSettingsError::AccountChanged));
            assert_eq!(stages.load(Ordering::SeqCst), 0);
            assert!(!form.is_cancelled());
        });
    }

    #[test]
    fn completion_rechecks_scope_and_timeout_stops_pending_work() {
        runtime().block_on(async {
            let selections = Selections::default();
            selections.replace(Some("casey-local"));
            let selection = selections.for_account("casey-local").unwrap();
            let form = RoomSettingsCancellation::new();
            let result = guarded(&selection, &form, Duration::from_secs(1), async {
                form.cancel();
                Ok(7)
            })
            .await;
            assert_eq!(result, Err(RoomSettingsError::Cancelled));
            let form = RoomSettingsCancellation::new();
            let result: Result<()> =
                guarded(&selection, &form, Duration::ZERO, std::future::pending()).await;
            assert_eq!(result, Err(RoomSettingsError::TimedOut));
        });
    }

    #[test]
    fn full_state_response_is_bound_to_exact_room_type_and_state_key() {
        let original = json!({"event_id":"$fixture","room_id":"!settings:example.org","sender":"@casey:example.org","type":"m.room.avatar","state_key":"","content":{}});
        assert!(
            parse_state(
                &original.to_string(),
                room_id!("!settings:example.org"),
                &StateEventType::RoomAvatar,
                ""
            )
            .is_ok()
        );
        for (field, value) in [
            ("room_id", json!("!other:example.org")),
            ("type", json!("m.room.topic")),
            ("state_key", json!("other")),
            ("content", json!([])),
            ("event_id", json!("invalid")),
            ("sender", json!("invalid")),
        ] {
            let mut changed = original.clone();
            changed[field] = value;
            assert!(matches!(
                parse_state(
                    &changed.to_string(),
                    room_id!("!settings:example.org"),
                    &StateEventType::RoomAvatar,
                    ""
                ),
                Err(RoomSettingsError::InvalidState)
            ));
        }
        assert!(
            parse_state(
                "[]",
                room_id!("!settings:example.org"),
                &StateEventType::RoomAvatar,
                ""
            )
            .is_err()
        );
        assert!(
            parse_state(
                &" ".repeat(MAX_STATE_BYTES + 1),
                room_id!("!settings:example.org"),
                &StateEventType::RoomAvatar,
                ""
            )
            .is_err()
        );
    }

    #[test]
    fn per_field_baselines_preserve_unknown_content_and_do_not_claim_cas() {
        let avatar = Some(json!({"url":"mxc://example.org/image","org.example.metadata":1}));
        assert_eq!(
            require_baseline(
                &avatar,
                &Some(json!({"org.example.metadata":1,"url":"mxc://example.org/image"}))
            ),
            Ok(())
        );
        assert_eq!(
            require_baseline(&avatar, &Some(json!({"url":"mxc://example.org/image"}))),
            Err(RoomSettingsError::BaselineChanged)
        );
        assert_eq!(
            require_baseline(&avatar, &None),
            Err(RoomSettingsError::BaselineChanged)
        );
        // Only the relevant field is compared; an unrelated topic edit is not
        // a conflict. An intervening edit back to equal content is unobservable.
        assert_eq!(require_baseline(&avatar, &avatar.clone()), Ok(()));
    }

    #[test]
    fn available_rules_follow_room_version_authorization_not_labels() {
        assert_eq!(
            rule_options(&RoomVersionId::V1),
            vec![JoinRuleKind::Invite, JoinRuleKind::Public]
        );
        assert_eq!(
            rule_options(&RoomVersionId::V7),
            vec![
                JoinRuleKind::Invite,
                JoinRuleKind::Public,
                JoinRuleKind::Knock
            ]
        );
        assert!(rule_options(&RoomVersionId::V8).contains(&JoinRuleKind::Restricted));
        assert!(!rule_options(&RoomVersionId::V8).contains(&JoinRuleKind::KnockRestricted));
        assert!(rule_options(&RoomVersionId::V10).contains(&JoinRuleKind::KnockRestricted));
        let unknown = RoomVersionId::try_from("org.example.future").unwrap();
        assert!(rule_options(&unknown).is_empty());
    }

    #[test]
    fn per_event_permissions_and_v12_creator_privilege_use_pinned_rules() {
        let join = envelope(StateEventType::RoomJoinRules, json!({"join_rule":"invite"}));
        let power = envelope(
            StateEventType::RoomPowerLevels,
            json!({
                "users":{"@jules:example.org":50},"state_default":100,
                "events":{"m.room.name":40,"m.room.avatar":60,"m.room.join_rules":50}
            }),
        );
        let first = loaded(&creation("11"), Some(&power), Some(&join));
        assert_eq!(
            first.snapshot.permissions,
            RoomSettingsPermissions {
                name: true,
                topic: false,
                avatar: false,
                join_rule: true
            }
        );
        let creators = envelope(
            StateEventType::RoomCreate,
            json!({"room_version":"12","additional_creators":["@jules:example.org"]}),
        );
        let privileged = loaded(&creators, Some(&power), Some(&join));
        assert_eq!(
            privileged.snapshot.permissions,
            RoomSettingsPermissions {
                name: true,
                topic: true,
                avatar: true,
                join_rule: true
            }
        );
        let unknown = loaded(&creation("org.example.future"), Some(&power), Some(&join));
        assert_eq!(
            unknown.snapshot.permissions,
            RoomSettingsPermissions::default()
        );
    }

    #[test]
    fn unknown_or_lossy_join_rules_remain_read_only() {
        for value in [
            json!({"join_rule":"custom","allow":[]}),
            json!({"join_rule":"invite","org.example.policy":true}),
            json!({"join_rule":"restricted","allow":[{"type":"custom","room_id":"!a:example.org"}]}),
            json!({"join_rule":"restricted","allow":[{"type":"m.room_membership","room_id":"!a:example.org","extension":true}]}),
            json!({"join_rule":"restricted","allow":[{}]}),
            json!({"join_rule":"restricted","allow":{}}),
        ] {
            assert!(matches!(
                current_rule(Some(&value), &RoomVersionId::V12),
                CurrentJoinRule::ReadOnly(_)
            ));
        }
        assert_eq!(
            current_rule(Some(&json!({"join_rule":"private"})), &RoomVersionId::V12),
            CurrentJoinRule::ReadOnly(ReadOnlyJoinRule::Private)
        );
        assert_eq!(
            current_rule(None, &RoomVersionId::V12),
            CurrentJoinRule::ReadOnly(ReadOnlyJoinRule::Missing)
        );
        assert_eq!(
            current_rule(Some(&json!({"join_rule":"knock"})), &RoomVersionId::V1),
            CurrentJoinRule::ReadOnly(ReadOnlyJoinRule::UnsupportedRoomVersion)
        );
        let join = envelope(
            StateEventType::RoomJoinRules,
            json!({"join_rule":"private"}),
        );
        let power = envelope(
            StateEventType::RoomPowerLevels,
            json!({"users":{"@jules:example.org":100}}),
        );
        assert!(
            !loaded(&creation("11"), Some(&power), Some(&join))
                .snapshot
                .permissions
                .join_rule
        );
    }

    #[test]
    fn restricted_rules_roundtrip_validated_legacy_and_serverless_ids() {
        for rule in [
            EditableJoinRule::Restricted {
                allow_rooms: vec!["!space:example.org".into(), "!serverless_room".into()],
            },
            EditableJoinRule::KnockRestricted {
                allow_rooms: vec!["!space:example.org".into()],
            },
        ] {
            let wire =
                serde_json::to_value(RoomJoinRulesEventContent::new(rule.wire().unwrap())).unwrap();
            assert_eq!(
                current_rule(Some(&wire), &RoomVersionId::V12),
                CurrentJoinRule::Editable(rule)
            );
        }
        assert_eq!(
            current_rule(
                Some(&json!({"join_rule":"restricted"})),
                &RoomVersionId::V12
            ),
            CurrentJoinRule::Editable(EditableJoinRule::Restricted {
                allow_rooms: vec![]
            })
        );
        for rooms in [
            vec!["not-a-room".into()],
            vec!["!same:example.org".into(), "!same:example.org".into()],
            vec![format!("!{}", "a".repeat(MAX_ROOM_ID_BYTES))],
            (0..=MAX_RESTRICTED_ALLOW_ROOMS)
                .map(|i| format!("!r{i}:example.org"))
                .collect(),
        ] {
            assert!(matches!(
                allowed_rooms(&rooms),
                Err(RoomSettingsError::InvalidJoinRule)
            ));
        }
    }

    #[test]
    fn avatar_summary_never_exposes_url_and_invalid_source_cannot_be_downloaded() {
        let create = creation("11");
        let avatar = envelope(
            StateEventType::RoomAvatar,
            json!({"url":"mxc://example.org/avatar","info":{"w":12,"h":34}}),
        );
        let value = assemble(
            &user_id!("@casey:example.org").to_owned(),
            &create,
            None,
            Some(&avatar),
            None,
            None,
            None,
            true,
        )
        .unwrap();
        assert_eq!(
            value.snapshot.avatar,
            RoomAvatarSummary {
                present: true,
                preview_available: true,
                width: Some(12),
                height: Some(34)
            }
        );
        assert!(value.snapshot.encrypted);
        assert!(!format!("{:?}", value.snapshot).contains("mxc://"));
        let invalid = envelope(
            StateEventType::RoomAvatar,
            json!({"url":"https://example.org/private"}),
        );
        let value = assemble(
            &user_id!("@casey:example.org").to_owned(),
            &create,
            None,
            Some(&invalid),
            None,
            None,
            None,
            false,
        )
        .unwrap();
        assert!(value.avatar_source.is_none());
        assert!(!value.snapshot.avatar.preview_available);
    }

    #[test]
    fn avatar_state_uses_bounded_components_without_leaking_sources() {
        let create = creation("11");
        let read = |address: &str| {
            let avatar = envelope(StateEventType::RoomAvatar, json!({"url": address}));
            assemble(
                &user_id!("@casey:example.org").to_owned(),
                &create,
                None,
                Some(&avatar),
                None,
                None,
                None,
                true,
            )
            .unwrap()
        };
        for labels in 36..=40 {
            let host = format!("{}.example", vec!["aaaaa"; labels].join("."));
            for port in [1, 11, 111, 1111, 11111] {
                let server = format!("{host}:{port}");
                let address = format!("mxc://{server}/avatar");
                let value = read(&address);
                assert!(value.snapshot.avatar.present);
                assert!(value.snapshot.avatar.preview_available);
                assert!(value.avatar_source.is_some());
                assert!(!format!("{:?}", value.snapshot).contains(&server));
            }
        }
        let oversized = format!("mxc://{}/avatar", "a".repeat(513));
        for address in [
            "https://example.org/private",
            "file:///private",
            "mxc://example.org/",
            "mxc://example.org/a/b",
            "mxc://example.org/é",
            oversized.as_str(),
        ] {
            let value = read(address);
            assert!(value.snapshot.avatar.present);
            assert!(!value.snapshot.avatar.preview_available);
            assert!(value.avatar_source.is_none());
            assert!(!format!("{:?}", value.snapshot).contains(address));
        }
    }

    #[test]
    fn encrypted_avatar_preview_requires_explicit_opt_in() {
        assert_eq!(
            require_preview_policy(true, false),
            Err(RoomSettingsError::EncryptedPreviewDisabled)
        );
        assert_eq!(require_preview_policy(true, true), Ok(()));
        assert_eq!(require_preview_policy(false, false), Ok(()));
        assert_eq!(require_preview_policy(false, true), Ok(()));
    }

    #[test]
    fn session_and_its_network_futures_keep_the_ui_worker_send_contract() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<RoomSettingsSession>();
        send_sync::<RoomSettingsCancellation>();
        // Compile-time proof only; no client, account, device, or service is opened.
        #[allow(dead_code)]
        fn futures(
            session: &RoomSettingsSession,
            manager: &AccountManager,
            selection: &AccountSelection,
            image: AvatarPng,
        ) {
            fn send<T: Send>(_: T) {}
            send(manager.open_room_settings_for(
                selection,
                "!settings:example.org",
                RoomSettingsCancellation::new(),
            ));
            send(session.refresh());
            send(session.set_name("Example room"));
            send(session.set_topic("Example topic"));
            send(session.set_avatar(image));
            send(session.remove_avatar());
            send(session.set_join_rule(EditableJoinRule::Invite));
            send(session.download_avatar_preview(false));
        }
    }

    #[test]
    fn editable_room_text_uses_byte_limits_and_preserves_topic_whitespace() {
        assert_eq!(editable_text("  Example room  ", true), Ok("Example room"));
        assert_eq!(
            editable_text(" \n ", true),
            Err(RoomSettingsError::InvalidText)
        );
        assert!(editable_text(&"é".repeat(512), true).is_ok());
        assert_eq!(
            editable_text(&"é".repeat(513), true),
            Err(RoomSettingsError::InvalidText)
        );
        assert_eq!(editable_text("", false), Ok(""));
        assert_eq!(editable_text("  topic\n", false), Ok("  topic\n"));
        assert!(editable_text(&"x".repeat(16 * 1024), false).is_ok());
        assert_eq!(
            editable_text(&"x".repeat(16 * 1024 + 1), false),
            Err(RoomSettingsError::InvalidText)
        );
    }

    #[test]
    fn saving_one_field_does_not_accept_an_unseen_edit_to_another() {
        let join = envelope(StateEventType::RoomJoinRules, json!({"join_rule":"invite"}));
        let mut previous = loaded(&creation("11"), None, Some(&join));
        previous.name = Some(json!({"name":"Before"}));
        previous.topic = Some(json!({"topic":"Before topic"}));
        previous.snapshot.name = "Before".into();
        previous.snapshot.topic = "Before topic".into();
        let mut fresh = previous.clone();
        fresh.name = Some(json!({"name":"After"}));
        fresh.topic = Some(json!({"topic":"Remote topic"}));
        fresh.avatar = Some(json!({"url":"mxc://example.org/new"}));
        fresh.join_rule = Some(json!({"join_rule":"public"}));
        fresh.snapshot.name = "After".into();
        fresh.snapshot.topic = "Remote topic".into();
        fresh.snapshot.avatar.present = true;
        fresh.snapshot.join_rule = CurrentJoinRule::Editable(EditableJoinRule::Public);
        fresh.snapshot.permissions.name = true;
        fresh.snapshot.encrypted = true;
        for saved in [
            SettingsField::Name,
            SettingsField::Topic,
            SettingsField::Avatar,
            SettingsField::JoinRule,
        ] {
            let published = preserve_other_fields(&previous, fresh.clone(), saved);
            let expected_name = if saved == SettingsField::Name {
                &fresh
            } else {
                &previous
            };
            let expected_topic = if saved == SettingsField::Topic {
                &fresh
            } else {
                &previous
            };
            let expected_avatar = if saved == SettingsField::Avatar {
                &fresh
            } else {
                &previous
            };
            let expected_join = if saved == SettingsField::JoinRule {
                &fresh
            } else {
                &previous
            };
            assert_eq!(published.name, expected_name.name);
            assert_eq!(published.snapshot.name, expected_name.snapshot.name);
            assert_eq!(published.topic, expected_topic.topic);
            assert_eq!(published.snapshot.topic, expected_topic.snapshot.topic);
            assert_eq!(published.avatar, expected_avatar.avatar);
            assert_eq!(published.snapshot.avatar, expected_avatar.snapshot.avatar);
            assert_eq!(published.join_rule, expected_join.join_rule);
            assert_eq!(
                published.snapshot.join_rule,
                expected_join.snapshot.join_rule
            );
            assert!(published.snapshot.permissions.name && published.snapshot.encrypted);
        }
    }

    // A real 1×1 metadata-free RGBA PNG made with the standard zlib encoder.
    const PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240,
        31, 0, 5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    #[test]
    fn canonical_png_is_bounded_and_dimensions_are_bound_to_its_header() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert!(AvatarPng::new(PNG.to_vec(), 1, 1).is_ok());
        for dimensions in [
            (0, 1),
            (2, 1),
            (MAX_ROOM_AVATAR_SIDE + 1, 1),
            (1, MAX_ROOM_AVATAR_SIDE + 1),
        ] {
            assert!(matches!(
                AvatarPng::new(PNG.to_vec(), dimensions.0, dimensions.1),
                Err(RoomSettingsError::InvalidAvatar)
            ));
        }
        assert!(matches!(
            AvatarPng::new(vec![0; MAX_ROOM_AVATAR_UPLOAD_BYTES + 1], 1, 1),
            Err(RoomSettingsError::InvalidAvatar)
        ));
    }

    #[test]
    fn avatar_rejects_corruption_metadata_animation_truncation_and_trailing_data() {
        for length in 0..PNG.len() {
            assert!(AvatarPng::new(PNG[..length].to_vec(), 1, 1).is_err());
        }
        let mut corrupt = PNG.to_vec();
        corrupt[20] ^= 1;
        assert!(AvatarPng::new(corrupt, 1, 1).is_err());
        let mut trailing = PNG.to_vec();
        trailing.push(0);
        assert!(AvatarPng::new(trailing, 1, 1).is_err());
        for kind in [b"tEXt", b"acTL", b"iCCP"] {
            let mut chunk = Vec::from(0u32.to_be_bytes());
            chunk.extend_from_slice(kind);
            chunk.extend_from_slice(&crc32(kind).to_be_bytes());
            let mut injected = PNG[..33].to_vec();
            injected.extend_from_slice(&chunk);
            injected.extend_from_slice(&PNG[33..]);
            assert!(AvatarPng::new(injected, 1, 1).is_err());
        }
    }
}
