//! Revocable, exact-room settings sessions. Reads are authoritative server GETs;
//! per-field baseline checks detect observed changes, never promise atomic CAS.
//! Room-avatar state is shared metadata even when the room's messages are encrypted.
//!
//! Each write (name, topic, join rule, avatar, avatar removal) is one state
//! request, and an avatar adds one media upload before it, a typed request
//! like the writes (the SDK's own upload first reads the server's size limit,
//! unbounded). Both are sent at most once: no HTTP retry, no automatic
//! authentication refresh and replay
//! (`disable_auth_refresh`, STAGED from root's SDK checkpoint d4c5d08, so this
//! compiles only on c7d0e12 combined with it), a timeout and a 64 KiB answer,
//! on the account's no-redirect transport. The final checks run after the
//! last await, right before the hand-off: the form is open, the exact
//! selection is current, the room is joined on this device, the app's
//! admission (its security gate) allows it, and the fresh permission read
//! just before allowed it. Every wait races the form's cancellation first,
//! then `AccountSelection::guard` (root's d56ba8b lets a ready revocation win
//! before the work is polled). Before the hand-off a stop sends nothing.
//! After it, a stop, a timeout or an answer other than a 4xx is `Unknown`
//! (the setting may or may not have changed), never resent; a refresh shows
//! what the room has. An avatar upload handed over without an answer is
//! `AvatarUploadUnknown`: the image may be stored, and no state was sent.

use std::{
    collections::BTreeSet,
    future::Future,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::Duration,
};

use matrix_sdk::{
    HttpError, Room, RoomState,
    config::RequestConfig,
    ruma::{
        OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedUserId, RoomId, RoomVersionId, UInt,
        api::{
            client::{
                media::create_content,
                state::{get_state_event_for_key, send_state_event},
            },
            error::ErrorKind,
        },
        events::{
            StateEventType,
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
use serde_json::{Value, value::RawValue};
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
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(45);
const WRITE_TIMEOUT: Duration = Duration::from_secs(120);
/// Most bytes accepted in a write's or an upload's answer: an event ID, a
/// content URI or an error.
const MAX_WRITE_RESPONSE_BYTES: u64 = 64 * 1024;

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
    #[error("Finish the account or security step in progress first; nothing was sent")]
    SecurityBusy,
    #[error("The homeserver refused this change; nothing was changed")]
    Refused,
    #[error("This sign-in needs renewing; the homeserver did not apply the change")]
    SignInExpired,
    #[error("The image could not be uploaded; the room avatar was not changed")]
    UploadFailed,
    #[error("No answer came back; the setting may or may not have changed")]
    Unknown,
    #[error("The image was uploaded, but whether the room avatar changed is unknown")]
    AvatarUnknown,
    #[error("The change was saved, but reading the settings back failed; refresh to see them")]
    SavedNotRead,
    #[error("The image was uploaded, but the room avatar was not changed")]
    AvatarUploadedNotChanged,
    #[error("The image may have reached the homeserver, but the room avatar was not changed")]
    AvatarUploadUnknown,
}
type Result<T> = std::result::Result<T, RoomSettingsError>;

// A room's addresses: aliases and the published addresses, on the same
// bounded requests, raw state and form contracts as the settings above.
mod room_addresses;
pub use room_addresses::{
    MAX_ALIAS_LOCALPART_CHARS, MAX_ALTERNATIVE_ADDRESSES, MAX_ROOM_ADDRESSES, PublishEdit,
    RoomAddress, RoomAddressError, RoomAddressesSession, RoomAddressesSnapshot, new_local_address,
};

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
    /// The validated PNG and its dimensions, for another core upload path
    /// (the account's own avatar). Validation already happened in `new`.
    pub(crate) fn into_parts(self) -> (Vec<u8>, u32, u32) {
        (self.bytes, self.width, self.height)
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
    raw: RawContents,
    avatar_source: Option<MediaSource>,
}

/// Each editable field's stored content as the exact text the server sent,
/// `None` without the event: the baseline a save must still find byte for
/// byte, and the content a save edits.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RawContents {
    name: Option<String>,
    topic: Option<String>,
    avatar: Option<String>,
    join_rule: Option<String>,
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
        fresh.raw.name = previous.raw.name.clone();
        fresh.snapshot.name = previous.snapshot.name.clone();
    }
    if saved != SettingsField::Topic {
        fresh.raw.topic = previous.raw.topic.clone();
        fresh.snapshot.topic = previous.snapshot.topic.clone();
    }
    if saved != SettingsField::Avatar {
        fresh.raw.avatar = previous.raw.avatar.clone();
        fresh.avatar_source = previous.avatar_source.clone();
        fresh.snapshot.avatar = previous.snapshot.avatar.clone();
    }
    if saved != SettingsField::JoinRule {
        fresh.raw.join_rule = previous.raw.join_rule.clone();
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
    admission: Option<Admission>,
}

/// Asked right before each request of a write is handed to the homeserver:
/// false refuses it with nothing sent.
type Admission = Arc<dyn Fn() -> bool + Send + Sync>;

/// What one write has done so far, so that a stop or a failure is reported
/// by what may have reached the homeserver, never as nothing sent.
#[derive(Default)]
struct Dispatch {
    /// The avatar image's upload: APPLIED once its address is confirmed.
    upload: AtomicU8,
    state: AtomicU8,
}
/// `Dispatch::state` and `Dispatch::upload` (0, the default: not handed
/// over): handed to the SDK, so it may or may not apply.
const SENT: u8 = 1;
/// Answered with a 4xx: not applied.
const REFUSED: u8 = 2;
/// Answered with success: applied.
const APPLIED: u8 = 3;

impl Dispatch {
    fn outcome(&self, error: RoomSettingsError) -> RoomSettingsError {
        let upload = self.upload.load(Ordering::Acquire);
        match self.state.load(Ordering::Acquire) {
            SENT if upload == APPLIED => RoomSettingsError::AvatarUnknown,
            SENT => RoomSettingsError::Unknown,
            APPLIED => RoomSettingsError::SavedNotRead,
            // Unsent or refused after a confirmed upload: the image did
            // leave this device, though no room state changed.
            _ if upload == APPLIED => RoomSettingsError::AvatarUploadedNotChanged,
            // An upload handed over and never answered (stopped, timed out,
            // unreadable, too large or a 5xx): the image may be stored, and
            // no room state was sent. Never "nothing was uploaded".
            _ if upload == SENT => RoomSettingsError::AvatarUploadUnknown,
            // Unsent or refused: nothing changed, and the error says why.
            _ => error,
        }
    }
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
            admission: None,
        })
    }
}

impl RoomSettingsSession {
    /// Every write asks `admission` right before each of its requests is
    /// handed to the homeserver, after every await; false refuses the write
    /// with nothing sent (`SecurityBusy`). The app passes its security gate.
    pub fn with_admission(mut self, admission: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        self.admission = Some(admission);
        self
    }
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
    /// A write under `run`: once its state request was handed over, a stop
    /// or a failure is reported by what it may have done.
    async fn run_write<T>(
        &self,
        dispatch: &Dispatch,
        work: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        self.run(WRITE_TIMEOUT, work)
            .await
            .map_err(|error| dispatch.outcome(error))
    }
    /// The checks right before a request is handed to the homeserver, after
    /// the last await: the form is open, the exact selection is current, the
    /// room is joined on this device and the app admits a change.
    fn admit(&self) -> Result<()> {
        self.check()?;
        if self.admission.as_ref().is_some_and(|admitted| !admitted()) {
            return Err(RoomSettingsError::SecurityBusy);
        }
        Ok(())
    }
    /// The one state request of a write, its content sent as the exact text
    /// built by `edited_content`. Nothing is awaited between the final checks
    /// and the hand-off.
    async fn send_state(
        &self,
        dispatch: &Dispatch,
        kind: StateEventType,
        content: Box<RawValue>,
    ) -> Result<()> {
        let room_id = self.room.room_id().to_owned();
        let content = matrix_sdk::ruma::serde::Raw::from_json(content);
        let request = send_state_event::v3::Request::new_raw(room_id, kind, String::new(), content);
        let client = self.room.client();
        self.admit()?;
        dispatch.state.store(SENT, Ordering::Release);
        let sent = client
            .send(request)
            .with_request_config(write_config())
            .await;
        match sent {
            Ok(_) => {
                dispatch.state.store(APPLIED, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                let refused = refusal(error.as_client_api_error());
                if refused.is_some() {
                    dispatch.state.store(REFUSED, Ordering::Release);
                }
                Err(refused.unwrap_or(RoomSettingsError::Unknown))
            }
        }
    }
    /// The avatar's one media upload, a typed request on the bounded upload
    /// config. The SDK's media upload would first read the server's size
    /// limit (`/media/config`, on the default config, unbounded) after the
    /// final checks; here nothing is awaited between them and the hand-off.
    /// Once handed over, only a 4xx answer means nothing was stored.
    async fn upload_avatar(&self, dispatch: &Dispatch, bytes: Vec<u8>) -> Result<OwnedMxcUri> {
        let mut request = create_content::v3::Request::new(bytes);
        request.content_type = Some("image/png".to_owned());
        let client = self.room.client();
        self.admit()?;
        dispatch.upload.store(SENT, Ordering::Release);
        let sent = client
            .send(request)
            .with_request_config(upload_config())
            .await;
        match sent {
            Ok(response) => {
                dispatch.upload.store(APPLIED, Ordering::Release);
                Ok(response.content_uri)
            }
            Err(error) => match refusal(error.as_client_api_error()) {
                Some(refused) => {
                    dispatch.upload.store(REFUSED, Ordering::Release);
                    Err(refused)
                }
                None => Err(RoomSettingsError::AvatarUploadUnknown),
            },
        }
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
        let dispatch = Dispatch::default();
        self.run_write(&dispatch, async {
            let baseline = self
                .loaded
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .raw
                .name
                .clone();
            let fresh = read_settings(&self.room, &self.own_user).await?;
            require_baseline(&baseline, &fresh.raw.name)?;
            if !fresh.snapshot.permissions.name {
                return Err(RoomSettingsError::PermissionDenied);
            }
            let typed = RoomNameEventContent::new(name.to_owned());
            let content = edited_content(fresh.raw.name.as_deref(), &["name"], typed)?;
            self.send_state(&dispatch, StateEventType::RoomName, content)
                .await?;
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
        let dispatch = Dispatch::default();
        self.run_write(&dispatch, async {
            let baseline = self
                .loaded
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .raw
                .topic
                .clone();
            let fresh = read_settings(&self.room, &self.own_user).await?;
            require_baseline(&baseline, &fresh.raw.topic)?;
            if !fresh.snapshot.permissions.topic {
                return Err(RoomSettingsError::PermissionDenied);
            }
            // Both the plain and the rich form are rewritten, so neither
            // keeps the old topic.
            let typed = RoomTopicEventContent::new(topic.to_owned());
            let stored = fresh.raw.topic.as_deref();
            let content = edited_content(stored, &["topic", "m.topic"], typed)?;
            self.send_state(&dispatch, StateEventType::RoomTopic, content)
                .await?;
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
        let dispatch = Dispatch::default();
        self.run_write(&dispatch, async {
            let baseline = self
                .loaded
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .raw
                .join_rule
                .clone();
            let fresh = read_settings(&self.room, &self.own_user).await?;
            require_baseline(&baseline, &fresh.raw.join_rule)?;
            if !matches!(fresh.snapshot.join_rule, CurrentJoinRule::Editable(_)) {
                return Err(RoomSettingsError::ReadOnly);
            }
            if !fresh.snapshot.available_join_rules.contains(&rule.kind()) {
                return Err(RoomSettingsError::InvalidJoinRule);
            }
            if !fresh.snapshot.permissions.join_rule {
                return Err(RoomSettingsError::PermissionDenied);
            }
            // An editable rule has no other fields (unknown structure is
            // read-only), so the stored allow list is replaced, not merged.
            let typed = RoomJoinRulesEventContent::new(wire);
            let stored = fresh.raw.join_rule.as_deref();
            let content = edited_content(stored, &["join_rule", "allow"], typed)?;
            self.send_state(&dispatch, StateEventType::RoomJoinRules, content)
                .await?;
            self.check()?;
            self.publish_write(
                read_settings(&self.room, &self.own_user).await?,
                SettingsField::JoinRule,
            )
        })
        .await
    }
    pub async fn set_avatar(&self, image: AvatarPng) -> Result<RoomSettingsSnapshot> {
        let dispatch = Dispatch::default();
        self.run_write(&dispatch, async {
            let baseline = self
                .loaded
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .raw
                .avatar
                .clone();
            self.avatar_permission(&baseline).await?;
            // Deliberately split upload and state write. A revoked form can
            // leave an unused upload, but never starts the later state change.
            let size = image.bytes.len();
            let content_uri = self.upload_avatar(&dispatch, image.bytes).await?;
            self.check()?;
            crate::media::validate_media_address(&content_uri)
                .map_err(|_| RoomSettingsError::InvalidState)?;
            let stored = self.avatar_permission(&baseline).await?;
            let mut info = ImageInfo::new();
            info.width = Some(UInt::from(image.width));
            info.height = Some(UInt::from(image.height));
            info.size = Some(UInt::try_from(size).map_err(|_| RoomSettingsError::InvalidAvatar)?);
            info.mimetype = Some("image/png".into());
            let mut avatar = RoomAvatarEventContent::new();
            avatar.url = Some(content_uri);
            avatar.info = Some(Box::new(info));
            let content = edited_content(stored.as_deref(), &["url", "info"], avatar)?;
            self.send_state(&dispatch, StateEventType::RoomAvatar, content)
                .await?;
            self.check()?;
            self.publish_write(
                read_settings(&self.room, &self.own_user).await?,
                SettingsField::Avatar,
            )
        })
        .await
    }
    /// The fresh avatar content as stored, once the permission and the
    /// baseline allow a change.
    async fn avatar_permission(&self, baseline: &Option<String>) -> Result<Option<String>> {
        let fresh = read_settings(&self.room, &self.own_user).await?;
        self.check()?;
        require_baseline(baseline, &fresh.raw.avatar)?;
        if !fresh.snapshot.permissions.avatar {
            return Err(RoomSettingsError::PermissionDenied);
        }
        Ok(fresh.raw.avatar)
    }
    pub async fn remove_avatar(&self) -> Result<RoomSettingsSnapshot> {
        let dispatch = Dispatch::default();
        self.run_write(&dispatch, async {
            let baseline = self
                .loaded
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .raw
                .avatar
                .clone();
            let stored = self.avatar_permission(&baseline).await?;
            let typed = RoomAvatarEventContent::new();
            let content = edited_content(stored.as_deref(), &["url", "info"], typed)?;
            self.send_state(&dispatch, StateEventType::RoomAvatar, content)
                .await?;
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

fn ensure_scope(
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
fn ensure_joined(room: &Room) -> Result<()> {
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
    // The form first, then the shared selection guard.
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(RoomSettingsError::Cancelled),
        result = selection.guard(time::timeout(limit, work)) => match result {
            Some(result) => result.map_err(|_| RoomSettingsError::TimedOut)?,
            None => Err(RoomSettingsError::AccountChanged),
        },
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
/// The stored content must be exactly the reviewed text: a value differing
/// only in a number's spelling, or reformatted, counts as a change.
fn require_baseline(expected: &Option<String>, current: &Option<String>) -> Result<()> {
    if expected != current {
        return Err(RoomSettingsError::BaselineChanged);
    }
    Ok(())
}

#[derive(Deserialize)]
struct StateEnvelope {
    #[serde(rename = "event_id")]
    _event_id: OwnedEventId,
    room_id: OwnedRoomId,
    sender: OwnedUserId,
    #[serde(rename = "type")]
    event_type: StateEventType,
    state_key: String,
    content: Value,
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
/// A state event whose content is kept exactly as the server sent it.
#[derive(Deserialize)]
struct RawStateEnvelope {
    #[serde(rename = "event_id")]
    _event_id: OwnedEventId,
    room_id: OwnedRoomId,
    #[serde(rename = "type")]
    event_type: StateEventType,
    state_key: String,
    content: Box<RawValue>,
}
fn parse_raw_state(
    raw: &str,
    room: &RoomId,
    kind: &StateEventType,
    key: &str,
) -> Result<Box<RawValue>> {
    if raw.len() > MAX_STATE_BYTES {
        return Err(RoomSettingsError::InvalidState);
    }
    let event: RawStateEnvelope =
        serde_json::from_str(raw).map_err(|_| RoomSettingsError::InvalidState)?;
    if event.room_id.as_str() != room.as_str()
        || &event.event_type != kind
        || event.state_key != key
        || !event.content.get().trim_start().starts_with('{')
    {
        return Err(RoomSettingsError::InvalidState);
    }
    Ok(event.content)
}
fn missing(error: &HttpError) -> bool {
    error.as_client_api_error().is_some_and(|e| {
        e.status_code.as_u16() == 404 && e.error_kind() == Some(&ErrorKind::NotFound)
    })
}
// Keep only a fixed category. Never retain reflected response bodies, URLs or
// server-provided Retry-After text, and never retry a state write automatically.
fn request_failure(error: HttpError) -> RoomSettingsError {
    // A state event larger than the read limit is not one Morpheus accepts.
    if matches!(error, HttpError::ResponseTooLarge { .. }) {
        return RoomSettingsError::InvalidState;
    }
    if error.as_client_api_error().is_some_and(|error| {
        error.status_code.as_u16() == 429
            || matches!(error.error_kind(), Some(ErrorKind::LimitExceeded(_)))
    }) {
        RoomSettingsError::RateLimited
    } else {
        RoomSettingsError::Unavailable
    }
}
/// One state read: no SDK retries or authentication refresh/replay, a
/// timeout, and refused while its body arrives once it grows past what
/// `parse_state` accepts, rather than read in full first. Room settings,
/// Space placement and its power-level checks all read state through it.
fn read_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .disable_auth_refresh()
        .timeout(REQUEST_TIMEOUT)
        .max_response_bytes(MAX_STATE_BYTES as u64)
}

async fn state(room: &Room, kind: StateEventType, key: &str) -> Result<Option<StateEnvelope>> {
    let mut request = get_state_event_for_key::v3::Request::new(
        room.room_id().to_owned(),
        kind.clone(),
        key.into(),
    );
    request.format = get_state_event_for_key::v3::StateEventFormat::Event;
    match room
        .client()
        .send(request)
        .with_request_config(read_config())
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
/// One attempt on the wire for a state write: no HTTP retry, no automatic
/// authentication refresh and replay (STAGED: root's d4c5d08), a timeout and
/// a small answer.
fn write_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .disable_auth_refresh()
        .timeout(REQUEST_TIMEOUT)
        .max_response_bytes(MAX_WRITE_RESPONSE_BYTES)
}
/// The same for the avatar's media upload, with the upload's timeout.
fn upload_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .disable_auth_refresh()
        .timeout(UPLOAD_TIMEOUT)
        .max_response_bytes(MAX_WRITE_RESPONSE_BYTES)
}
/// A definite 4xx answer, which applied nothing, as a fixed category. Any
/// other failure may have applied. Server text is never kept.
fn refusal(answer: Option<&matrix_sdk::ruma::api::error::Error>) -> Option<RoomSettingsError> {
    let answer = answer?;
    if !answer.status_code.is_client_error() {
        return None;
    }
    Some(match answer.error_kind() {
        Some(ErrorKind::LimitExceeded(_)) => RoomSettingsError::RateLimited,
        _ if answer.status_code.as_u16() == 429 => RoomSettingsError::RateLimited,
        Some(ErrorKind::UnknownToken(_)) => RoomSettingsError::SignInExpired,
        Some(ErrorKind::Forbidden) => RoomSettingsError::PermissionDenied,
        _ => RoomSettingsError::Refused,
    })
}
/// One editable field's state event (the same bounded read as `state`),
/// parsed for the snapshot and kept as the exact content text the server
/// sent, for the baseline and the edit. Both `None` without the event.
async fn state_and_raw(
    room: &Room,
    kind: StateEventType,
) -> Result<(Option<StateEnvelope>, Option<String>)> {
    let mut request = get_state_event_for_key::v3::Request::new(
        room.room_id().to_owned(),
        kind.clone(),
        "".into(),
    );
    request.format = get_state_event_for_key::v3::StateEventFormat::Event;
    let response = room
        .client()
        .send(request)
        .with_request_config(read_config())
        .await;
    match response {
        Ok(response) => {
            let text = response.event_or_content.get();
            let event = parse_state(text, room.room_id(), &kind, "")?;
            let content = parse_raw_state(text, room.room_id(), &kind, "")?;
            Ok((Some(event), Some(content.get().to_owned())))
        }
        Err(error) if missing(&error) => Ok((None, None)),
        Err(error) => Err(request_failure(error)),
    }
}
/// A content object's fields as raw text, in stored order. A normal map
/// keeps only one of two equal names; such content is refused instead.
struct ContentFields(Vec<(String, Box<RawValue>)>);
impl<'de> Deserialize<'de> for ContentFields {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> serde::de::Visitor<'de> for FieldsVisitor {
            type Value = ContentFields;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an object with unique field names")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut fields: Vec<(String, Box<RawValue>)> = Vec::new();
                while let Some((name, value)) = map.next_entry::<String, Box<RawValue>>()? {
                    if fields.iter().any(|(seen, _)| *seen == name) {
                        return Err(serde::de::Error::custom("duplicate field"));
                    }
                    fields.push((name, value));
                }
                Ok(ContentFields(fields))
            }
        }
        deserializer.deserialize_map(FieldsVisitor)
    }
}
fn content_fields(text: &str) -> Result<Vec<(String, Box<RawValue>)>> {
    let fields = serde_json::from_str::<ContentFields>(text);
    fields
        .map(|fields| fields.0)
        .map_err(|_| RoomSettingsError::InvalidState)
}
/// `stored` (one content object's exact text; `None` without the event) with
/// the fields of `typed` (what Morpheus writes) set in place or appended,
/// and every other name in `replaced` dropped. Every other field keeps its
/// exact text and order: unknown extensions, numbers beyond u64 and
/// decimals included. Content that is not an object or repeats a name is
/// refused, never rewritten, and nothing is sent.
fn edited_content(
    stored: Option<&str>,
    replaced: &[&str],
    typed: impl serde::Serialize,
) -> Result<Box<RawValue>> {
    let invalid = |_| RoomSettingsError::InvalidState;
    let mut fields = match stored {
        None => Vec::new(),
        Some(text) => content_fields(text)?,
    };
    let Value::Object(typed) = serde_json::to_value(typed).map_err(invalid)? else {
        return Err(RoomSettingsError::InvalidState);
    };
    fields.retain(|(name, _)| !replaced.contains(&name.as_str()) || typed.contains_key(name));
    for (name, value) in typed {
        let value = RawValue::from_string(value.to_string()).map_err(invalid)?;
        match fields.iter_mut().find(|(existing, _)| *existing == name) {
            Some((_, slot)) => *slot = value,
            None => fields.push((name, value)),
        }
    }
    let mut text = String::from("{");
    for (index, (name, value)) in fields.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        text.push_str(&serde_json::to_string(name).map_err(invalid)?);
        text.push(':');
        text.push_str(value.get());
    }
    text.push('}');
    if text.len() > MAX_STATE_BYTES {
        return Err(RoomSettingsError::InvalidState);
    }
    RawValue::from_string(text).map_err(invalid)
}
fn content<T: serde::de::DeserializeOwned>(event: &StateEnvelope) -> Result<T> {
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
    let (avatar, avatar_raw) = state_and_raw(room, StateEventType::RoomAvatar).await?;
    let (join, join_raw) = state_and_raw(room, StateEventType::RoomJoinRules).await?;
    let (name, name_raw) = state_and_raw(room, StateEventType::RoomName).await?;
    let (topic, topic_raw) = state_and_raw(room, StateEventType::RoomTopic).await?;
    let encryption = state(room, StateEventType::RoomEncryption, "").await?;
    ensure_joined(room)?;
    let mut loaded = assemble(
        own,
        &create,
        power.as_ref(),
        avatar.as_ref(),
        join.as_ref(),
        name.as_ref(),
        topic.as_ref(),
        encryption.is_some(),
    )?;
    loaded.raw = RawContents {
        name: name_raw,
        topic: topic_raw,
        avatar: avatar_raw,
        join_rule: join_raw,
    };
    Ok(loaded)
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
    if let Some(levels) = power_levels(create, power)? {
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
        // The exact stored text is filled in by `read_settings`.
        raw: RawContents::default(),
        avatar_source,
    })
}
fn state_text(event: Option<&StateEnvelope>, key: &str, maximum: usize) -> Result<String> {
    match event.and_then(|e| e.content.get(key)) {
        None => Ok(String::new()),
        Some(Value::String(s)) if s.len() <= maximum => Ok(s.clone()),
        _ => Err(RoomSettingsError::InvalidState),
    }
}
/// The power levels of a room from its create and power-level state, with
/// the creators its room version names. `None` for a room version these
/// rules do not cover, which grants nothing.
fn power_levels(
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
    Ok(Some(RoomPowerLevels::new(
        power,
        &rules.authorization,
        creators,
    )))
}

/// Whether `own` may send `kind` state in the joined `room` now, from a
/// fresh read of its create and power-level state (no SDK retries).
pub(crate) async fn can_send_state(
    room: &Room,
    own: &OwnedUserId,
    kind: StateEventType,
) -> Result<bool> {
    ensure_joined(room)?;
    let create = state(room, StateEventType::RoomCreate, "")
        .await?
        .ok_or(RoomSettingsError::InvalidState)?;
    let power = state(room, StateEventType::RoomPowerLevels, "").await?;
    ensure_joined(room)?;
    Ok(power_levels(&create, power.as_ref())?
        .is_some_and(|levels| levels.user_can_send_state(own, kind)))
}

/// The content of one state event of the joined `room`, read fresh (bounded,
/// no SDK retries), or `None` when there is none. Every field is kept, so a
/// write built from it loses nothing another client stored.
pub(crate) async fn state_content(
    room: &Room,
    kind: StateEventType,
    key: &str,
) -> Result<Option<Value>> {
    ensure_joined(room)?;
    Ok(state(room, kind, key).await?.map(|event| event.content))
}

/// One state event's content as the server sent it, every value byte for
/// byte (numbers beyond u64 and decimals included), for a write that keeps
/// what it does not change. The same bounded read as state_content; `None`
/// without the event.
pub(crate) async fn state_content_raw(
    room: &Room,
    kind: StateEventType,
    key: &str,
) -> Result<Option<Box<RawValue>>> {
    ensure_joined(room)?;
    let mut request = get_state_event_for_key::v3::Request::new(
        room.room_id().to_owned(),
        kind.clone(),
        key.into(),
    );
    request.format = get_state_event_for_key::v3::StateEventFormat::Event;
    match room
        .client()
        .send(request)
        .with_request_config(read_config())
        .await
    {
        Ok(response) => {
            parse_raw_state(response.event_or_content.get(), room.room_id(), &kind, key).map(Some)
        }
        Err(error) if missing(&error) => Ok(None),
        Err(error) => Err(request_failure(error)),
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
    use crate::history::tests::{fixture, runtime as history_runtime};
    use matrix_sdk::ruma::{room_id, user_id};
    use serde_json::json;
    use std::{
        collections::{BTreeMap, VecDeque},
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::{Mutex as StdMutex, atomic::AtomicUsize},
    };

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
    fn raw_state_content_keeps_every_value_as_sent() {
        let raw = r#"{"event_id":"$fixture","room_id":"!space:example.org","sender":"@casey:example.org","type":"m.space.child","state_key":"!child:example.org","content":{"via":["example.org"],"org.example.big":184467440737095516160,"org.example.rate":1.10}}"#;
        let (space, kind) = (room_id!("!space:example.org"), StateEventType::SpaceChild);
        let content = parse_raw_state(raw, space, &kind, "!child:example.org").unwrap();
        // Beyond u64 and a decimal with its trailing zero, byte for byte.
        assert!(content.get().contains("184467440737095516160"));
        assert!(content.get().contains("1.10"));
        // Bound to the exact room, type and state key, and to an object.
        let child = "!child:example.org";
        let other = room_id!("!other:example.org");
        assert!(parse_raw_state(raw, other, &kind, child).is_err());
        assert!(parse_raw_state(raw, space, &kind, "!other:example.org").is_err());
        assert!(parse_raw_state(raw, space, &StateEventType::RoomTopic, child).is_err());
        let listed = raw
            .replace(r#""content":{"#, r#""content":[{"#)
            .replace("}}", "}]}");
        assert!(parse_raw_state(&listed, space, &kind, child).is_err());
        let oversized = " ".repeat(MAX_STATE_BYTES + 1);
        assert!(parse_raw_state(&oversized, space, &kind, child).is_err());
    }

    #[test]
    fn per_field_baselines_compare_the_exact_stored_text_and_do_not_claim_cas() {
        let text = |raw: &str| Some(raw.to_owned());
        let avatar = text(r#"{"url":"mxc://example.org/image","org.example.metadata":1.10}"#);
        assert_eq!(require_baseline(&avatar, &avatar.clone()), Ok(()));
        // Equal as parsed values, yet a different stored text: a change,
        // never silently accepted (a number's spelling, field order).
        for changed in [
            r#"{"url":"mxc://example.org/image","org.example.metadata":1.1}"#,
            r#"{"org.example.metadata":1.10,"url":"mxc://example.org/image"}"#,
            r#"{"url":"mxc://example.org/image"}"#,
        ] {
            assert_eq!(
                require_baseline(&avatar, &text(changed)),
                Err(RoomSettingsError::BaselineChanged),
                "{changed}"
            );
        }
        assert_eq!(
            require_baseline(&avatar, &None),
            Err(RoomSettingsError::BaselineChanged)
        );
        // Only the relevant field is compared; an unrelated topic edit is not
        // a conflict. An intervening edit back to equal content is unobservable.
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
        previous.raw.name = Some(r#"{"name":"Before"}"#.into());
        previous.raw.topic = Some(r#"{"topic":"Before topic"}"#.into());
        previous.snapshot.name = "Before".into();
        previous.snapshot.topic = "Before topic".into();
        let mut fresh = previous.clone();
        fresh.raw.name = Some(r#"{"name":"After"}"#.into());
        fresh.raw.topic = Some(r#"{"topic":"Remote topic"}"#.into());
        fresh.raw.avatar = Some(r#"{"url":"mxc://example.org/new"}"#.into());
        fresh.raw.join_rule = Some(r#"{"join_rule":"public"}"#.into());
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
            assert_eq!(published.raw.name, expected_name.raw.name);
            assert_eq!(published.snapshot.name, expected_name.snapshot.name);
            assert_eq!(published.raw.topic, expected_topic.raw.topic);
            assert_eq!(published.snapshot.topic, expected_topic.snapshot.topic);
            assert_eq!(published.raw.avatar, expected_avatar.raw.avatar);
            assert_eq!(published.snapshot.avatar, expected_avatar.snapshot.avatar);
            assert_eq!(published.raw.join_rule, expected_join.raw.join_rule);
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

    // Writes against a loopback homeserver: one joined room of one account
    // with a restored synthetic session. No real account or network.
    const ROOM: &str = "!settings:example.org";
    const ME: &str = "@casey:example.org";

    #[derive(Clone, Default)]
    struct Served {
        /// Each state event's content as stored text, served byte for byte.
        state: Arc<StdMutex<BTreeMap<(String, String), String>>>,
        syncs: Arc<StdMutex<VecDeque<Value>>>,
        /// Each write's event type and body as sent.
        writes: Arc<StdMutex<Vec<(String, String)>>>,
        write_reply: Arc<StdMutex<Option<(u16, Value)>>>,
        upload_reply: Arc<StdMutex<Option<(u16, Value)>>>,
        uploads: Arc<AtomicUsize>,
        /// An upload taken while uploads are held is never answered.
        hold_uploads: Arc<AtomicBool>,
        /// Reads of the server's media limits, which are never served.
        media_config: Arc<AtomicUsize>,
        power_reads: Arc<AtomicUsize>,
        hold_power: Arc<AtomicBool>,
        /// The power-level read with this number waits until it is reset.
        hold_power_read: Arc<AtomicUsize>,
        hold_writes: Arc<AtomicBool>,
        // Address regressions reuse this same bounded fixture transport.
        state_read_reply: Arc<StdMutex<Option<(u16, String)>>>,
        refused_state_reads: Arc<AtomicUsize>,
        refresh_reads: Arc<AtomicUsize>,
        alias_reads: Arc<AtomicUsize>,
        stopped: Arc<AtomicBool>,
    }

    /// It answers the version check, queued syncs, state reads from its
    /// current state, state writes (recorded, then applied unless a reply is
    /// set) and media uploads, each connection on its own thread. Power-level
    /// reads can be held until released; a state write or an upload taken
    /// while held is never answered or applied, like a lost connection. It
    /// has no media limits (`/media/config`): it only counts their reads.
    struct SettingsServer {
        url: String,
        served: Served,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl SettingsServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let served = Served::default();
            *served.state.lock().unwrap() = initial_state();
            let serving = served.clone();
            let worker = std::thread::spawn(move || {
                while !serving.stopped.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let serving = serving.clone();
                            std::thread::spawn(move || answer(stream, &serving));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                url,
                served,
                worker: Some(worker),
            }
        }

        fn writes(&self) -> Vec<(String, String)> {
            self.served.writes.lock().unwrap().clone()
        }

        fn uploads(&self) -> usize {
            self.served.uploads.load(Ordering::SeqCst)
        }

        fn media_config_reads(&self) -> usize {
            self.served.media_config.load(Ordering::SeqCst)
        }

        async fn wait_for_uploads(&self, count: usize) {
            while self.uploads() < count {
                time::sleep(Duration::from_millis(5)).await;
            }
        }

        fn power_reads(&self) -> usize {
            self.served.power_reads.load(Ordering::SeqCst)
        }

        async fn wait_for_writes(&self, count: usize) {
            while self.writes().len() < count {
                time::sleep(Duration::from_millis(5)).await;
            }
        }

        async fn wait_for_power_reads(&self, count: usize) {
            while self.power_reads() < count {
                time::sleep(Duration::from_millis(5)).await;
            }
        }
    }

    impl Drop for SettingsServer {
        fn drop(&mut self) {
            self.served.stopped.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn initial_state() -> BTreeMap<(String, String), String> {
        let mut state = BTreeMap::new();
        let mut put = |kind: &str, key: &str, content: Value| {
            state.insert((kind.to_owned(), key.to_owned()), content.to_string());
        };
        let create = json!({"creator": ME, "room_version": "10"});
        put("m.room.create", "", create);
        put("m.room.member", ME, json!({"membership": "join"}));
        put("m.room.power_levels", "", json!({"users": {ME: 100}}));
        put("m.room.join_rules", "", json!({"join_rule": "invite"}));
        state
    }

    /// The whole state event, its content inserted as the stored text.
    fn full_event(kind: &str, key: &str, content: &str) -> String {
        let head = json!({
            "type": kind,
            "state_key": key,
            "sender": ME,
            "event_id": format!("$state-{kind}"),
            "room_id": ROOM,
            "origin_server_ts": 1,
        });
        let head = head.to_string();
        format!("{},\"content\":{content}}}", &head[..head.len() - 1])
    }

    /// The first sync: the account is joined to the room.
    fn joined_room() -> Value {
        let state = initial_state();
        let events: Vec<_> = state
            .iter()
            .filter(|((kind, _), _)| kind == "m.room.create" || kind == "m.room.member")
            .map(|((kind, key), content)| {
                serde_json::from_str::<Value>(&full_event(kind, key, content)).unwrap()
            })
            .collect();
        let mut join = serde_json::Map::new();
        let room = json!({
            "state": {"events": events},
            "timeline": {"events": [], "limited": false},
        });
        join.insert(ROOM.to_owned(), room);
        json!({"next_batch": "s1", "rooms": {"join": join}})
    }

    fn answer(mut stream: TcpStream, served: &Served) {
        stream.set_nonblocking(false).unwrap();
        let timeout = Some(Duration::from_secs(2));
        stream.set_read_timeout(timeout).unwrap();
        let (line, body) = read_request(&mut stream);
        let mut parts = line.split(' ');
        let method = parts.next().unwrap_or("").to_owned();
        let path = parts.next().unwrap_or("").to_owned();
        let not_found = json!({"errcode": "M_NOT_FOUND", "error": "x"});
        let (status, reply) = if path.starts_with("/_matrix/client/versions") {
            (200, json!({"versions": ["v1.15"]}).to_string())
        } else if path.contains("/refresh") {
            served.refresh_reads.fetch_add(1, Ordering::SeqCst);
            (
                200,
                json!({"access_token":"synthetic renewed token"}).to_string(),
            )
        } else if path.contains("/directory/room/") {
            served.alias_reads.fetch_add(1, Ordering::SeqCst);
            (404, not_found.to_string())
        } else if path.contains("/sync") {
            let next = served.syncs.lock().unwrap().pop_front();
            (
                200,
                next.unwrap_or_else(|| json!({"next_batch": "idle"}))
                    .to_string(),
            )
        } else if path.contains("/media/config") {
            served.media_config.fetch_add(1, Ordering::SeqCst);
            (
                404,
                json!({"errcode": "M_UNRECOGNIZED", "error": "x"}).to_string(),
            )
        } else if method == "POST" && path.contains("/upload") {
            served.uploads.fetch_add(1, Ordering::SeqCst);
            if served.hold_uploads.load(Ordering::SeqCst) {
                while !served.stopped.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                return;
            }
            let uploaded = json!({"content_uri": "mxc://example.org/avatar1"});
            let reply = served.upload_reply.lock().unwrap().clone();
            let (status, reply) = reply.unwrap_or((200, uploaded));
            (status, reply.to_string())
        } else if let Some((kind, key)) = state_target(&path) {
            if method == "PUT" {
                let write = (kind.clone(), body.clone());
                served.writes.lock().unwrap().push(write);
                if served.hold_writes.load(Ordering::SeqCst) {
                    while !served.stopped.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    return;
                }
                let reply = served.write_reply.lock().unwrap().clone();
                match reply {
                    Some((status, reply)) => (status, reply.to_string()),
                    None => {
                        served.state.lock().unwrap().insert((kind, key), body);
                        (200, json!({"event_id": "$written"}).to_string())
                    }
                }
            } else if let Some(reply) = served.state_read_reply.lock().unwrap().clone() {
                served.refused_state_reads.fetch_add(1, Ordering::SeqCst);
                reply
            } else {
                if kind == "m.room.power_levels" {
                    let read = served.power_reads.fetch_add(1, Ordering::SeqCst) + 1;
                    while served.hold_power.load(Ordering::SeqCst)
                        || served.hold_power_read.load(Ordering::SeqCst) == read
                    {
                        if served.stopped.load(Ordering::SeqCst) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                let entry = (kind.clone(), key.clone());
                let content = served.state.lock().unwrap().get(&entry).cloned();
                match content {
                    Some(content) => (200, full_event(&kind, &key, &content)),
                    None => (404, not_found.to_string()),
                }
            }
        } else {
            (
                404,
                json!({"errcode": "M_UNRECOGNIZED", "error": "x"}).to_string(),
            )
        };
        let response = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        let _ = stream.write_all(response.as_bytes());
    }

    /// The request line and body of one HTTP/1.1 request.
    fn read_request(stream: &mut TcpStream) -> (String, String) {
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Ok(size) = stream.read(&mut chunk) {
            if size == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..size]);
            let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..end]).to_string();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    let length = name.eq_ignore_ascii_case("content-length");
                    length.then(|| value.trim().parse::<usize>().ok()).flatten()
                })
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                let line = headers.lines().next().unwrap_or("").to_owned();
                let body = String::from_utf8_lossy(&request[end + 4..end + 4 + length]);
                return (line, body.to_string());
            }
        }
        (String::new(), String::new())
    }

    /// The decoded event type and state key of a state request path.
    fn state_target(path: &str) -> Option<(String, String)> {
        let path = path.split('?').next()?;
        let (_, rest) = path.split_once("/state/")?;
        let (kind, key) = rest.split_once('/').unwrap_or((rest, ""));
        Some((decode(kind)?, decode(key)?))
    }

    fn decode(segment: &str) -> Option<String> {
        let mut decoded = Vec::new();
        let mut rest = segment.as_bytes();
        while let Some((&first, tail)) = rest.split_first() {
            if first == b'%' && tail.len() >= 2 {
                let hex = std::str::from_utf8(&tail[..2]).ok()?;
                decoded.push(u8::from_str_radix(hex, 16).ok()?);
                rest = &tail[2..];
            } else {
                decoded.push(first);
                rest = tail;
            }
        }
        String::from_utf8(decoded).ok()
    }

    /// A fresh manager whose selected account is signed in with a synthetic
    /// session against `server` and synced once into the joined room.
    async fn signed_in(
        server: &SettingsServer,
    ) -> (std::path::PathBuf, Arc<AccountManager>, crate::AccountInfo) {
        signed_in_with_refresh(server, false).await
    }

    async fn signed_in_with_refresh(
        server: &SettingsServer,
        refresh: bool,
    ) -> (std::path::PathBuf, Arc<AccountManager>, crate::AccountInfo) {
        let account = crate::AccountInfo {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: ME.into(),
            device_id: "SETTINGS-FIXTURE".into(),
            homeserver: server.url.clone(),
        };
        let (directory, manager, account) = fixture(Some(account)).await;
        if refresh {
            let client = matrix_sdk::Client::builder()
                .homeserver_url(&server.url)
                .handle_refresh_tokens()
                .build()
                .await
                .unwrap();
            client.send_queue().set_enabled(false).await;
            let mut state = manager.state.lock().await;
            // The fixture has not shared its LoadedAccount with any worker.
            Arc::get_mut(state.loaded.get_mut(&account.id).unwrap())
                .unwrap()
                .client = client;
        }
        let client = {
            let state = manager.state.lock().await;
            state.loaded[&account.id].client.clone()
        };
        let session = matrix_sdk::authentication::matrix::MatrixSession {
            meta: matrix_sdk::SessionMeta {
                user_id: user_id!("@casey:example.org").to_owned(),
                device_id: "SETTINGS-FIXTURE".into(),
            },
            tokens: matrix_sdk::SessionTokens {
                access_token: "synthetic access token".into(),
                refresh_token: refresh.then(|| "synthetic refresh token".to_owned()),
            },
        };
        let restored = client
            .matrix_auth()
            .restore_session(session, matrix_sdk::store::RoomLoadSettings::default())
            .await;
        restored.unwrap();
        server.served.syncs.lock().unwrap().push_back(joined_room());
        let settings = matrix_sdk::config::SyncSettings::default();
        client.sync_once(settings).await.unwrap();
        (directory, manager, account)
    }

    /// A form for the room under the account's current selection, admitted
    /// while `gate` is true.
    async fn open(
        manager: &AccountManager,
        account: &crate::AccountInfo,
        gate: &Arc<AtomicBool>,
        form: RoomSettingsCancellation,
    ) -> RoomSettingsSession {
        let selection = manager.account_selection(&account.id).unwrap();
        let session = manager.open_room_settings_for(&selection, ROOM, form).await;
        let gate = gate.clone();
        session
            .unwrap()
            .with_admission(Arc::new(move || gate.load(Ordering::SeqCst)))
    }

    #[test]
    fn addresses_withdraw_during_manager_or_permission_wait_and_reject_epoch_aba() {
        history_runtime().block_on(async {
            let server = SettingsServer::start();
            let (directory, manager, account) = signed_in(&server).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let token = RoomSettingsCancellation::new();
            let held = manager.state.lock().await;
            let close = async {
                tokio::task::yield_now().await;
                token.cancel();
            };
            let (result, ()) = tokio::join!(
                manager.open_room_addresses_for(&selection, ROOM, token.clone(), Arc::new(|| true)),
                close
            );
            assert!(matches!(result, Err(RoomAddressError::Cancelled)));
            drop(held);
            let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let expected = epoch.clone();
            let token = RoomSettingsCancellation::new();
            let session = manager
                .open_room_addresses_for(
                    &selection,
                    ROOM,
                    token.clone(),
                    Arc::new(move || expected.load(Ordering::SeqCst) == 0),
                )
                .await
                .unwrap();
            for withdraw in [false, true] {
                server.served.hold_power.store(true, Ordering::SeqCst);
                let count = server.power_reads();
                let revoke = async {
                    server.wait_for_power_reads(count + 1).await;
                    if withdraw {
                        token.cancel();
                    } else {
                        epoch.store(2, Ordering::SeqCst);
                    }
                    server.served.hold_power.store(false, Ordering::SeqCst);
                };
                let (result, ()) =
                    tokio::join!(session.edit_published(PublishEdit::ClearMain), revoke);
                assert_eq!(
                    result.err(),
                    Some(if withdraw {
                        RoomAddressError::Cancelled
                    } else {
                        RoomAddressError::SecurityBusy
                    })
                );
                epoch.store(0, Ordering::SeqCst);
            }
            assert!(server.writes().is_empty());
            drop(session);
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn addresses_cancel_after_dispatch_keeps_unknown_and_promotion_checks_mapping() {
        history_runtime().block_on(async {
            let server = SettingsServer::start();
            let (directory, manager, account) = signed_in(&server).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let token = RoomSettingsCancellation::new();
            let session = manager
                .open_room_addresses_for(&selection, ROOM, token.clone(), Arc::new(|| true))
                .await
                .unwrap();
            server.served.hold_writes.store(true, Ordering::SeqCst);
            let close = async {
                server.wait_for_writes(1).await;
                token.cancel();
            };
            let (result, ()) = tokio::join!(session.edit_published(PublishEdit::ClearMain), close);
            assert_eq!(result.err(), Some(RoomAddressError::Unknown));
            assert_eq!(server.writes().len(), 1);
            drop(session);
            server.served.hold_writes.store(false, Ordering::SeqCst);
            server.served.state.lock().unwrap().insert(
                ("m.room.canonical_alias".into(), "".into()),
                r##"{"alt_aliases":["#old:example.org"],"extension":1.10}"##.into(),
            );
            let session = manager
                .open_room_addresses_for(
                    &selection,
                    ROOM,
                    RoomSettingsCancellation::new(),
                    Arc::new(|| true),
                )
                .await
                .unwrap();
            let result = session
                .edit_published(PublishEdit::SetMain("#old:example.org".into()))
                .await;
            assert_eq!(result.err(), Some(RoomAddressError::NotFound));
            assert_eq!(server.served.alias_reads.load(Ordering::SeqCst), 1);
            assert_eq!(
                server.writes().len(),
                1,
                "promotion must not publish an unresolved mapping"
            );
            drop(session);
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn settings_expired_sign_in_does_not_refresh_or_replay_its_bounded_state_read() {
        history_runtime().block_on(async {
            let server = SettingsServer::start();
            let (directory, manager, account) = signed_in_with_refresh(&server, true).await;
            *server.served.state_read_reply.lock().unwrap() = Some((
                401,
                json!({"errcode":"M_UNKNOWN_TOKEN","error":"synthetic private text"}).to_string(),
            ));
            let selection = manager.account_selection(&account.id).unwrap();
            let result = manager
                .open_room_settings_for(&selection, ROOM, RoomSettingsCancellation::new())
                .await;
            // The family's existing fixed category is preserved; no server body.
            assert!(matches!(result, Err(RoomSettingsError::Unavailable)));
            assert_eq!(server.served.refused_state_reads.load(Ordering::SeqCst), 1);
            assert_eq!(server.served.refresh_reads.load(Ordering::SeqCst), 0);
            assert!(server.writes().is_empty());
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn addresses_expired_sign_in_does_not_refresh_or_replay_its_bounded_read() {
        history_runtime().block_on(async {
            let server = SettingsServer::start();
            let (directory, manager, account) = signed_in_with_refresh(&server, true).await;
            *server.served.state_read_reply.lock().unwrap() = Some((
                401,
                json!({"errcode":"M_UNKNOWN_TOKEN","error":"synthetic private text"}).to_string(),
            ));
            let selection = manager.account_selection(&account.id).unwrap();
            let result = manager
                .open_room_addresses_for(
                    &selection,
                    ROOM,
                    RoomSettingsCancellation::new(),
                    Arc::new(|| true),
                )
                .await;
            assert!(matches!(result, Err(RoomAddressError::SignInExpired)));
            assert_eq!(server.served.refresh_reads.load(Ordering::SeqCst), 0);
            assert!(server.writes().is_empty());
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn every_write_is_one_bounded_request_and_an_avatar_adds_one_upload() {
        for config in [write_config(), upload_config()] {
            let config = format!("{config:?}");
            assert!(config.contains("retry_limit: 0"), "{config}");
            assert!(config.contains("max_response_bytes: 65536"), "{config}");
            // STAGED: root's d4c5d08.
            assert!(config.contains("disable_auth_refresh: true"), "{config}");
        }
        history_runtime().block_on(async {
            let server = SettingsServer::start();
            let (directory, manager, account) = signed_in(&server).await;
            let gate = Arc::new(AtomicBool::new(true));
            let form = RoomSettingsCancellation::new();
            let session = open(&manager, &account, &gate, form).await;
            assert_eq!(session.set_name("Plans").await.unwrap().name, "Plans");
            session.set_topic("Weekly plans").await.unwrap();
            let knock = EditableJoinRule::Knock;
            session.set_join_rule(knock).await.unwrap();
            let image = AvatarPng::new(PNG.to_vec(), 1, 1).unwrap();
            assert!(session.set_avatar(image).await.unwrap().avatar.present);
            assert!(!session.remove_avatar().await.unwrap().avatar.present);
            let writes = server.writes();
            let kinds: Vec<_> = writes.iter().map(|(kind, _)| kind.as_str()).collect();
            assert_eq!(
                kinds,
                [
                    "m.room.name",
                    "m.room.topic",
                    "m.room.join_rules",
                    "m.room.avatar",
                    "m.room.avatar",
                ]
            );
            assert_eq!(writes[0].1, r#"{"name":"Plans"}"#);
            assert_eq!(writes[2].1, r#"{"join_rule":"knock"}"#);
            assert_eq!(server.uploads(), 1);
            // The upload is the typed request alone: no media-limits read.
            assert_eq!(server.media_config_reads(), 0);
            drop(session);
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn a_closed_gate_or_a_changed_account_during_the_permission_read_sends_nothing() {
        history_runtime().block_on(async {
            let server = SettingsServer::start();
            let (directory, manager, account) = signed_in(&server).await;
            let gate = Arc::new(AtomicBool::new(true));
            let form = RoomSettingsCancellation::new();
            let session = open(&manager, &account, &gate, form).await;
            // The security gate closes while the fresh permission read waits.
            server.served.hold_power.store(true, Ordering::SeqCst);
            let reads = server.power_reads();
            let close = async {
                server.wait_for_power_reads(reads + 1).await;
                gate.store(false, Ordering::SeqCst);
                server.served.hold_power.store(false, Ordering::SeqCst);
            };
            let (result, ()) = tokio::join!(session.set_name("Plans"), close);
            assert_eq!(result.err(), Some(RoomSettingsError::SecurityBusy));
            // An avatar is refused before its upload as well.
            let image = AvatarPng::new(PNG.to_vec(), 1, 1).unwrap();
            let refused = session.set_avatar(image).await;
            assert_eq!(refused.err(), Some(RoomSettingsError::SecurityBusy));
            assert_eq!(server.uploads(), 0);
            // The account changes while the permission read waits.
            gate.store(true, Ordering::SeqCst);
            server.served.hold_power.store(true, Ordering::SeqCst);
            let reads = server.power_reads();
            let switch = async {
                server.wait_for_power_reads(reads + 1).await;
                manager.publish_active_account(None);
                server.served.hold_power.store(false, Ordering::SeqCst);
            };
            let (result, ()) = tokio::join!(session.set_topic("Weekly plans"), switch);
            assert_eq!(result.err(), Some(RoomSettingsError::AccountChanged));
            assert!(server.writes().is_empty());
            drop(session);
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn a_write_handed_over_then_stopped_or_unanswered_is_unknown_and_never_resent() {
        use RoomSettingsError as Error;
        history_runtime().block_on(async {
            let server = SettingsServer::start();
            let (directory, manager, account) = signed_in(&server).await;
            let gate = Arc::new(AtomicBool::new(true));
            server.served.hold_writes.store(true, Ordering::SeqCst);
            // The form closes while the write is with the homeserver.
            let form = RoomSettingsCancellation::new();
            let session = open(&manager, &account, &gate, form.clone()).await;
            let close = async {
                server.wait_for_writes(1).await;
                form.cancel();
            };
            let (result, ()) = tokio::join!(session.set_name("Plans"), close);
            assert_eq!(result.err(), Some(Error::Unknown));
            // The account changes while the next one is with the homeserver.
            let form = RoomSettingsCancellation::new();
            let session = open(&manager, &account, &gate, form).await;
            let switch = async {
                server.wait_for_writes(2).await;
                manager.publish_active_account(None);
            };
            let (result, ()) = tokio::join!(session.set_topic("Weekly plans"), switch);
            assert_eq!(result.err(), Some(Error::Unknown));
            manager.publish_active_account(Some(account.clone()));
            // A confirmed upload, then a stopped state write: said exactly so.
            let form = RoomSettingsCancellation::new();
            let session = open(&manager, &account, &gate, form.clone()).await;
            let close = async {
                server.wait_for_writes(3).await;
                form.cancel();
            };
            let image = AvatarPng::new(PNG.to_vec(), 1, 1).unwrap();
            let (result, ()) = tokio::join!(session.set_avatar(image), close);
            assert_eq!(result.err(), Some(Error::AvatarUnknown));
            assert_eq!(server.uploads(), 1);
            // No answer within the request's timeout.
            let form = RoomSettingsCancellation::new();
            let session = open(&manager, &account, &gate, form).await;
            let unanswered = time::timeout(Duration::from_secs(30), session.set_name("Plans"));
            assert_eq!(unanswered.await.unwrap().err(), Some(Error::Unknown));
            // None of them was sent again.
            assert_eq!(server.writes().len(), 4);
            // Answers: a 4xx changed nothing; anything else may have.
            server.served.hold_writes.store(false, Ordering::SeqCst);
            let error = |code: &str| json!({"errcode": code, "error": "homeserver text"});
            // The unknown-token answer comes last: it marks the token expired.
            let answers = [
                (429, error("M_LIMIT_EXCEEDED"), Error::RateLimited),
                (403, error("M_FORBIDDEN"), Error::PermissionDenied),
                (400, error("M_BAD_JSON"), Error::Refused),
                (500, error("M_UNKNOWN"), Error::Unknown),
                (502, json!("not json"), Error::Unknown),
                (401, error("M_UNKNOWN_TOKEN"), Error::SignInExpired),
            ];
            let form = RoomSettingsCancellation::new();
            let session = open(&manager, &account, &gate, form).await;
            for (sent, (status, body, expected)) in answers.into_iter().enumerate() {
                if status == 401 {
                    // An upload the homeserver refuses changes nothing either.
                    let refused = Some((413, error("M_TOO_LARGE")));
                    *server.served.upload_reply.lock().unwrap() = refused;
                    let image = AvatarPng::new(PNG.to_vec(), 1, 1).unwrap();
                    let result = session.set_avatar(image).await;
                    assert_eq!(result.err(), Some(Error::Refused));
                    assert_eq!(server.uploads(), 2);
                    assert_eq!(server.writes().len(), 4 + sent);
                }
                *server.served.write_reply.lock().unwrap() = Some((status, body));
                let result = session.set_name("Plans").await;
                assert_eq!(result.err(), Some(expected), "status {status}");
                assert_eq!(server.writes().len(), 4 + sent + 1, "status {status}");
                assert!(!expected.to_string().contains("homeserver text"));
            }
            drop(session);
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn every_state_read_is_capped_at_the_transport_and_oversize_is_a_fixed_error() {
        let config = format!("{:?}", read_config());
        assert!(config.contains("retry_limit: 0"), "{config}");
        assert!(
            config.contains(&format!("max_response_bytes: {MAX_STATE_BYTES}")),
            "{config}"
        );
        // Refused while the body arrives; the label is fixed, no size or text.
        let too_large = HttpError::ResponseTooLarge {
            limit: MAX_STATE_BYTES as u64,
        };
        let failure = request_failure(too_large);
        assert_eq!(failure, RoomSettingsError::InvalidState);
        assert!(!failure.to_string().contains(&MAX_STATE_BYTES.to_string()));
    }

    #[test]
    fn an_edit_sets_only_the_known_fields_and_keeps_every_other_byte() {
        const BIG: &str = "123456789012345678901234567890";
        let named = |name: &str| RoomNameEventContent::new(name.to_owned());
        let stored = format!(r#"{{"name":"Old","org.example.big":{BIG},"org.example.x":1.10}}"#);
        let edited = edited_content(Some(&stored), &["name"], named("New")).unwrap();
        let expected = stored.replace(r#""name":"Old""#, r#""name":"New""#);
        assert_eq!(edited.get(), expected);
        // Both topic forms are rewritten: nothing of the old topic remains.
        let topic = concat!(
            r#"{"m.topic":{"m.text":[{"body":"Old"}]},"#,
            r#""topic":"Old","org.example.x":2.5E-3}"#,
        );
        let typed = RoomTopicEventContent::new("New".to_owned());
        let edited = edited_content(Some(topic), &["topic", "m.topic"], typed).unwrap();
        assert!(!edited.get().contains("Old"), "{}", edited.get());
        assert!(edited.get().contains(r#""org.example.x":2.5E-3"#));
        // A cleared avatar loses its address and description, nothing else.
        let avatar = r#"{"url":"mxc://example.org/a","info":{"w":1},"org.example.x":1}"#;
        let cleared = RoomAvatarEventContent::new();
        let cleared = edited_content(Some(avatar), &["url", "info"], cleared).unwrap();
        assert_eq!(cleared.get(), r#"{"org.example.x":1}"#);
        // Leaving a restricted rule drops its allow list.
        let restricted = r#"{"join_rule":"restricted","allow":[]}"#;
        let invite = RoomJoinRulesEventContent::new(JoinRule::Invite);
        let edited = edited_content(Some(restricted), &["join_rule", "allow"], invite).unwrap();
        assert_eq!(edited.get(), r#"{"join_rule":"invite"}"#);
        // Nothing stored yet: only the typed fields.
        let first = edited_content(None, &["name"], named("New")).unwrap();
        assert_eq!(first.get(), r#"{"name":"New"}"#);
        // Odd stored content is refused, never rewritten.
        for odd in ["[]", r#""text""#, r#"{"name":"a","name":"b"}"#] {
            let refused = edited_content(Some(odd), &["name"], named("New"));
            assert_eq!(
                refused.err(),
                Some(RoomSettingsError::InvalidState),
                "{odd}"
            );
        }
    }

    #[test]
    fn a_save_keeps_unknown_fields_byte_for_byte_and_a_respelled_baseline_is_a_change() {
        history_runtime().block_on(async {
            let server = SettingsServer::start();
            let stored = concat!(
                r#"{"name":"Old","org.example.rate":1.10,"#,
                r#""org.example.big":123456789012345678901234567890}"#,
            );
            let name = ("m.room.name".to_owned(), String::new());
            let state = server.served.state.clone();
            state
                .lock()
                .unwrap()
                .insert(name.clone(), stored.to_owned());
            let (directory, manager, account) = signed_in(&server).await;
            let gate = Arc::new(AtomicBool::new(true));
            let form = RoomSettingsCancellation::new();
            let session = open(&manager, &account, &gate, form).await;
            assert_eq!(session.snapshot().name, "Old");
            session.set_name("New").await.unwrap();
            let expected = stored.replace(r#""name":"Old""#, r#""name":"New""#);
            assert_eq!(server.writes()[0].1, expected);
            // Another client respells only a number: equal as values, yet a
            // different stored text, so nothing is sent.
            let respelled = expected.replace("1.10", "1.1");
            state.lock().unwrap().insert(name, respelled);
            let refused = session.set_name("Newer").await;
            assert_eq!(refused.err(), Some(RoomSettingsError::BaselineChanged));
            assert_eq!(server.writes().len(), 1);
            drop(session);
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn a_confirmed_upload_then_a_closed_gate_says_the_image_left_and_nothing_changed() {
        history_runtime().block_on(async {
            let server = SettingsServer::start();
            let (directory, manager, account) = signed_in(&server).await;
            let gate = Arc::new(AtomicBool::new(true));
            let form = RoomSettingsCancellation::new();
            let session = open(&manager, &account, &gate, form).await;
            // The permission read after the upload waits while the security
            // gate closes.
            let reads = server.power_reads();
            let held = &server.served.hold_power_read;
            held.store(reads + 2, Ordering::SeqCst);
            let close = async {
                server.wait_for_power_reads(reads + 2).await;
                gate.store(false, Ordering::SeqCst);
                held.store(0, Ordering::SeqCst);
            };
            let image = AvatarPng::new(PNG.to_vec(), 1, 1).unwrap();
            let (result, ()) = tokio::join!(session.set_avatar(image), close);
            let uploaded = RoomSettingsError::AvatarUploadedNotChanged;
            assert_eq!(result.err(), Some(uploaded));
            // The image left once; no state was sent and nothing is resent.
            assert_eq!(server.uploads(), 1);
            assert!(server.writes().is_empty());
            drop(session);
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn an_upload_handed_over_without_an_answer_may_be_stored_and_writes_no_state() {
        use RoomSettingsError as Error;
        let image = || AvatarPng::new(PNG.to_vec(), 1, 1).unwrap();
        history_runtime().block_on(async {
            let server = SettingsServer::start();
            let (directory, manager, account) = signed_in(&server).await;
            let gate = Arc::new(AtomicBool::new(true));
            // The gate closes during the permission read just before the
            // upload: the final check after it refuses, nothing leaves.
            let form = RoomSettingsCancellation::new();
            let session = open(&manager, &account, &gate, form).await;
            let reads = server.power_reads();
            let held = &server.served.hold_power_read;
            held.store(reads + 1, Ordering::SeqCst);
            let close = async {
                server.wait_for_power_reads(reads + 1).await;
                gate.store(false, Ordering::SeqCst);
                held.store(0, Ordering::SeqCst);
            };
            let (result, ()) = tokio::join!(session.set_avatar(image()), close);
            assert_eq!(result.err(), Some(Error::SecurityBusy));
            assert_eq!(server.uploads(), 0);
            gate.store(true, Ordering::SeqCst);
            // The form closes while the upload is with the homeserver.
            server.served.hold_uploads.store(true, Ordering::SeqCst);
            let form = RoomSettingsCancellation::new();
            let session = open(&manager, &account, &gate, form.clone()).await;
            let close = async {
                server.wait_for_uploads(1).await;
                form.cancel();
            };
            let (result, ()) = tokio::join!(session.set_avatar(image()), close);
            assert_eq!(result.err(), Some(Error::AvatarUploadUnknown));
            // The account changes while the next one is with the homeserver.
            let form = RoomSettingsCancellation::new();
            let session = open(&manager, &account, &gate, form).await;
            let switch = async {
                server.wait_for_uploads(2).await;
                manager.publish_active_account(None);
            };
            let (result, ()) = tokio::join!(session.set_avatar(image()), switch);
            assert_eq!(result.err(), Some(Error::AvatarUploadUnknown));
            manager.publish_active_account(Some(account.clone()));
            server.served.hold_uploads.store(false, Ordering::SeqCst);
            // Answers: only a 4xx says nothing was stored. A 5xx, one that is
            // not JSON or one past the answer limit may follow a stored image.
            let error = |code: &str| json!({"errcode": code, "error": "homeserver text"});
            let padding = "x".repeat(MAX_WRITE_RESPONSE_BYTES as usize);
            let too_large = json!({"content_uri": "mxc://example.org/avatar1", "pad": padding});
            let answers = [
                (500, error("M_UNKNOWN"), Error::AvatarUploadUnknown),
                (502, json!("not json"), Error::AvatarUploadUnknown),
                (200, too_large, Error::AvatarUploadUnknown),
                (413, error("M_TOO_LARGE"), Error::Refused),
            ];
            let form = RoomSettingsCancellation::new();
            let session = open(&manager, &account, &gate, form).await;
            for (sent, (status, body, expected)) in answers.into_iter().enumerate() {
                *server.served.upload_reply.lock().unwrap() = Some((status, body));
                let result = session.set_avatar(image()).await;
                assert_eq!(result.err(), Some(expected), "status {status}");
                // One upload per save, never resent.
                assert_eq!(server.uploads(), 2 + sent + 1, "status {status}");
                assert!(!expected.to_string().contains("homeserver text"));
            }
            assert!(!Error::AvatarUploadUnknown.to_string().contains("could not"));
            // No room state was sent, and no media limits were read.
            assert!(server.writes().is_empty());
            assert_eq!(server.media_config_reads(), 0);
            drop(session);
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }
}
