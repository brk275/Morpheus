use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use matrix_rtc_call_sdk::{LiveKitCall, LiveKitCallOptions, Participant, close_slot, open_slot};
use matrix_rtc_core::{LiveKitTransport, SlotEncryption, compat::MembershipFormat};
use matrix_rtc_transport::MediaStreamKind;
use matrix_sdk::{
    Client,
    ruma::{
        RoomId,
        api::{client::state::get_state_event_for_key, error::ErrorKind},
        events::StateEventType,
    },
};
use serde_json::Value;

use crate::{
    RtcError, TransportSupport, VOICE_SPACE_EVENT_TYPE, VOICE_SPACE_STATE_KEY, VoiceSpaceMarker,
    classify_transports,
};

/// The membership format to publish. Legacy formats exist for interoperation
/// with Element Call generations that have not adopted the current MSC4143
/// event shape. They are distinct calls, not a transparent fallback.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InteropMode {
    #[default]
    Current,
    Sticky2025,
    RoomState,
}

impl From<InteropMode> for MembershipFormat {
    fn from(value: InteropMode) -> Self {
        match value {
            InteropMode::Current => Self::Current,
            InteropMode::Sticky2025 => Self::Sticky2025,
            InteropMode::RoomState => Self::RoomState,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallOptions {
    pub slot_id: String,
    pub interop_mode: InteropMode,
}

impl Default for CallOptions {
    fn default() -> Self {
        Self {
            // The pinned RTC core's current room-wide slot sentinel.
            slot_id: "m.call#room".to_owned(),
            interop_mode: InteropMode::Current,
        }
    }
}

impl CallOptions {
    /// Element Call's Matrix 2.0 setting still speaks the 2025 sticky dialect.
    /// Keep the core's lowercase slot; the pinned adapter converts its wire
    /// membership and authorization request together. This is never a fallback.
    pub fn element_call_matrix_2_0() -> Self {
        Self {
            slot_id: "m.call#room".to_owned(),
            interop_mode: InteropMode::Sticky2025,
        }
    }

    fn validate_encryption(&self, encrypted: bool) -> Result<(), RtcError> {
        if !encrypted && self.interop_mode != InteropMode::RoomState {
            return Err(RtcError::UnencryptedRoomUnsupported);
        }
        Ok(())
    }

    /// Join the pre-sticky Element Call room-state dialect. Its empty legacy
    /// `call_id` maps to the RTC core's lowercase `room` sentinel. Element
    /// Call's current-mode uppercase `ROOM` slot has separate interoperability
    /// requirements and is not enabled by this option.
    pub fn room_state_compatibility() -> Self {
        Self {
            slot_id: "m.call#room".to_owned(),
            interop_mode: InteropMode::RoomState,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallInfo {
    pub room_id: String,
    pub slot_id: String,
    pub membership_id: String,
    pub local_identity: String,
}

/// Identity and lifetime of one join on one Matrix account and device.
/// Handles created for this join become invalid as soon as it leaves.
#[derive(Clone, Debug)]
pub(crate) struct CallLease {
    pub info: CallInfo,
    pub account_user_id: String,
    pub account_device_id: String,
    active: Arc<AtomicBool>,
}

impl CallLease {
    pub(crate) fn new(info: CallInfo, account_user_id: String, account_device_id: String) -> Self {
        Self {
            info,
            account_user_id,
            account_device_id,
            active: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn revoke(&self) {
        self.active.store(false, Ordering::Release);
    }

    pub fn same_join(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.active, &other.active)
            && self.info == other.info
            && self.account_user_id == other.account_user_id
            && self.account_device_id == other.account_device_id
    }
}

struct ActiveCall {
    call: LiveKitCall,
    lease: CallLease,
}

/// Morpheus-owned roster vocabulary. The call SDK's participant and stream
/// structs remain inside this integration boundary while they evolve.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RtcStreamKind {
    Microphone,
    Camera,
    ScreenShare,
    ScreenShareAudio,
    Data,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParticipantStreamInfo {
    pub kind: RtcStreamKind,
    pub muted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParticipantInfo {
    /// MatrixRTC membership key, unique for each join.
    pub member_id: String,
    pub user_id: String,
    pub device_id: Option<String>,
    pub is_local: bool,
    /// False when this participant advertises no transport this client can use.
    pub reachable: bool,
    pub streams: Vec<ParticipantStreamInfo>,
    pub hand_raised_at_ms: Option<u64>,
    pub joined_at_ms: Option<u64>,
}

impl From<Participant> for ParticipantInfo {
    fn from(participant: Participant) -> Self {
        Self {
            member_id: participant.member_id,
            user_id: participant.user_id,
            device_id: participant.device_id,
            is_local: participant.is_local,
            reachable: participant.reachable,
            streams: participant
                .streams
                .into_iter()
                .map(|stream| ParticipantStreamInfo {
                    kind: match stream.kind {
                        MediaStreamKind::Microphone => RtcStreamKind::Microphone,
                        MediaStreamKind::Camera => RtcStreamKind::Camera,
                        MediaStreamKind::ScreenShare => RtcStreamKind::ScreenShare,
                        MediaStreamKind::ScreenShareAudio => RtcStreamKind::ScreenShareAudio,
                        MediaStreamKind::Data => RtcStreamKind::Data,
                    },
                    muted: stream.muted,
                })
                .collect(),
            hand_raised_at_ms: participant.hand_raised_at_ms,
            joined_at_ms: participant.joined_at_ms,
        }
    }
}

/// One account's active native MatrixRTC calls.
///
/// The host must run matrix-sdk-ui's SyncService with MSC4354 sticky events,
/// keep this account's Tokio runtime running, and call `leave` before dropping
/// the service. MatrixRTC uses that sync feed for memberships and media keys.
/// `LiveKitCall` exposes frame streams. Optional device features add native
/// microphone capture, bounded group-audio playback, and video capture. The
/// host still owns permissions, video rendering, and device controls.
pub struct RtcService {
    client: Client,
    calls: HashMap<String, ActiveCall>,
}

impl RtcService {
    pub fn new(client: Client) -> Self {
        install_crypto_provider();
        Self {
            client,
            calls: HashMap::new(),
        }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Query the SDK's MSC4143 transport discovery (including its well-known
    /// fallback) for a usable native LiveKit focus. A failed request remains an
    /// error; absence is `NoLiveKitTransport`.
    pub async fn discover_transport(&self) -> Result<TransportSupport, RtcError> {
        install_crypto_provider();
        if self.client.user_id().is_none() {
            return Err(RtcError::SignedOut);
        }
        let advertised = self
            .client
            .discover_rtc_transports()
            .await
            .map_err(|error| RtcError::Discovery(error.to_string()))?
            .unwrap_or_default();
        let value = serde_json::to_value(advertised)
            .map_err(|error| RtcError::Discovery(error.to_string()))?;
        Ok(classify_transports(&value))
    }

    async fn ensure_sticky_support(&self) -> Result<(), RtcError> {
        if !self
            .client
            .supports_sticky_events()
            .await
            .map_err(|error| RtcError::Discovery(error.to_string()))?
        {
            return Err(RtcError::StickyEventsUnsupported);
        }
        Ok(())
    }

    /// Publish `m.rtc.slot` before starting a new call. This requires room
    /// state-event power. For encrypted rooms, use `encrypted = true` to select
    /// MatrixRTC's `m.per_member` media-key policy.
    pub async fn open_slot(
        &self,
        room_id: &str,
        slot_id: &str,
        encrypted: bool,
    ) -> Result<(), RtcError> {
        let room = self.ensure_loaded_room(room_id)?;
        let encryption_state = room
            .latest_encryption_state()
            .await
            .map_err(|error| RtcError::EncryptionReadiness(error.to_string()))?;
        if encryption_state.is_unknown() {
            return Err(RtcError::UnknownEncryptionState);
        }
        // Current MSC4143 cleartext memberships have no attributable sender
        // device. The pinned LiveKit transport needs that device to map a
        // membership onto the peer's pseudonymous SFU identity, so opening a
        // slot here would produce a roster with no remote media.
        if !encryption_state.is_encrypted() {
            return Err(RtcError::UnencryptedRoomUnsupported);
        }
        // Reject unsupported homeservers before writing a slot that nobody
        // using Morpheus can join. Joining performs the same discovery again
        // because the advertised service may change between these actions.
        if matches!(
            self.discover_transport().await?,
            TransportSupport::NoLiveKitTransport
        ) {
            return Err(RtcError::NoLiveKitTransport);
        }
        self.ensure_sticky_support().await?;
        if encrypted {
            self.ensure_encrypted_call_ready().await?;
        }
        let encryption = encrypted.then(|| SlotEncryption {
            encryption_type: "m.per_member".to_owned(),
            extra: Default::default(),
        });
        open_slot(&self.client, room_id, slot_id, "m.call", encryption).await?;
        Ok(())
    }

    /// Close a slot that this account has permission to manage. Peers in the
    /// slot are forced to leave by the MatrixRTC call engine.
    pub async fn close_slot(&self, room_id: &str, slot_id: &str) -> Result<(), RtcError> {
        self.ensure_loaded_room(room_id)?;
        close_slot(&self.client, room_id, slot_id).await?;
        Ok(())
    }

    /// Join a loaded room's open call slot, publishing MatrixRTC membership and
    /// connecting the native E2EE LiveKit media transport.
    pub async fn join(
        &mut self,
        room_id: &str,
        options: CallOptions,
    ) -> Result<CallInfo, RtcError> {
        let room = self.ensure_loaded_room(room_id)?;
        if self.calls.contains_key(room_id) {
            return Err(RtcError::AlreadyJoined);
        }
        let encryption_state = room
            .latest_encryption_state()
            .await
            .map_err(|error| RtcError::EncryptionReadiness(error.to_string()))?;
        if encryption_state.is_unknown() {
            return Err(RtcError::UnknownEncryptionState);
        }
        options.validate_encryption(encryption_state.is_encrypted())?;
        let service_url = match self.discover_transport().await? {
            TransportSupport::LiveKit { service_url } => service_url,
            TransportSupport::NoLiveKitTransport => return Err(RtcError::NoLiveKitTransport),
        };
        if options.interop_mode != InteropMode::RoomState {
            self.ensure_sticky_support().await?;
        }
        if encryption_state.is_encrypted() {
            self.ensure_encrypted_call_ready().await?;
        }
        let account_user_id = self
            .client
            .user_id()
            .ok_or(RtcError::SignedOut)?
            .to_string();
        let account_device_id = self
            .client
            .device_id()
            .ok_or(RtcError::SignedOut)?
            .to_string();

        install_crypto_provider();
        let call = LiveKitCall::join(
            &room,
            LiveKitCallOptions {
                slot_id: options.slot_id.clone(),
                format: options.interop_mode.into(),
                livekit_transport: Some(LiveKitTransport {
                    livekit_service_url: service_url,
                }),
                ..LiveKitCallOptions::default()
            },
        )
        .await?;
        let info = CallInfo {
            room_id: room_id.to_owned(),
            slot_id: options.slot_id,
            membership_id: call.membership_id().to_owned(),
            local_identity: call.local_identity().to_owned(),
        };
        let lease = CallLease::new(info.clone(), account_user_id, account_device_id);
        self.calls
            .insert(room_id.to_owned(), ActiveCall { call, lease });
        Ok(info)
    }

    /// Leave a call cleanly. The call is removed even when the Matrix leave
    /// send fails, because the upstream `leave` consumes its call handle.
    pub async fn leave(&mut self, room_id: &str) -> Result<(), RtcError> {
        let active = self.calls.remove(room_id).ok_or(RtcError::NotJoined)?;
        active.lease.revoke();
        active.call.leave().await?;
        Ok(())
    }

    pub fn call(&self, room_id: &str) -> Option<&LiveKitCall> {
        self.calls.get(room_id).map(|active| &active.call)
    }

    pub fn call_mut(&mut self, room_id: &str) -> Option<&mut LiveKitCall> {
        self.calls.get_mut(room_id).map(|active| &mut active.call)
    }

    /// The joined call's unified event stream: connection health, the end
    /// reason, and participant/stream changes used to reconcile after a
    /// transport reconnect.
    pub fn subscribe_call_events(
        &self,
        room_id: &str,
    ) -> Result<tokio::sync::broadcast::Receiver<crate::RtcCallEvent>, RtcError> {
        self.call(room_id)
            .map(LiveKitCall::subscribe_call_events)
            .ok_or(RtcError::NotJoined)
    }

    pub(crate) fn call_lease(&self, room_id: &str) -> Option<&CallLease> {
        self.calls.get(room_id).map(|active| &active.lease)
    }

    /// Latest MatrixRTC signalling roster, including our own membership.
    /// A caller can use `call(room_id).subscribe_participants()` as its change
    /// signal, then read this stable Morpheus snapshot after each update.
    pub fn participants(&self, room_id: &str) -> Result<Vec<ParticipantInfo>, RtcError> {
        let call = self.calls.get(room_id).ok_or(RtcError::NotJoined)?;
        Ok(call
            .call
            .participants()
            .into_iter()
            .map(Into::into)
            .collect())
    }

    /// Read the optional Morpheus presentation marker from the homeserver.
    /// Native sliding sync may omit custom state types, so a cached state
    /// lookup cannot reliably discover a marker another member just wrote.
    /// `None` means this room has no shared voice-space designation.
    /// MatrixRTC slots and room permissions remain authoritative for joining.
    pub async fn voice_space_marker(
        &self,
        room_id: &str,
    ) -> Result<Option<VoiceSpaceMarker>, RtcError> {
        let room = self.ensure_loaded_room(room_id)?;
        let request = get_state_event_for_key::v3::Request::new(
            room.room_id().to_owned(),
            StateEventType::from(VOICE_SPACE_EVENT_TYPE),
            VOICE_SPACE_STATE_KEY.to_owned(),
        );
        let response = match self.client.send(request).await {
            Ok(response) => response,
            Err(error) if error.client_api_error_kind() == Some(&ErrorKind::NotFound) => {
                return Ok(None);
            }
            Err(error) => return Err(RtcError::RoomState(error.to_string())),
        };
        let content: Value = serde_json::from_str(response.event_or_content.get())
            .map_err(|error| RtcError::RoomState(error.to_string()))?;
        Ok(Some(VoiceSpaceMarker::parse_content(&content)?))
    }

    /// Set the shared room hint when the current account has state-event
    /// power. The user must explicitly request this; a local designation can
    /// be used in settings instead when room power is unavailable.
    pub async fn set_voice_space_marker(
        &self,
        room_id: &str,
        enabled: bool,
    ) -> Result<(), RtcError> {
        let room = self.ensure_loaded_room(room_id)?;
        room.send_state_event_raw(
            VOICE_SPACE_EVENT_TYPE,
            VOICE_SPACE_STATE_KEY,
            VoiceSpaceMarker { enabled }.content(),
        )
        .await
        .map_err(|error| RtcError::RoomState(error.to_string()))?;
        Ok(())
    }

    pub fn active_room_ids(&self) -> impl Iterator<Item = &str> {
        self.calls.keys().map(String::as_str)
    }

    fn ensure_loaded_room(&self, room_id: &str) -> Result<matrix_sdk::Room, RtcError> {
        let room_id = RoomId::parse(room_id).map_err(|_| RtcError::InvalidRoomId)?;
        self.client
            .get_room(&room_id)
            .ok_or(RtcError::RoomNotLoaded)
    }

    async fn ensure_encrypted_call_ready(&self) -> Result<(), RtcError> {
        // The pinned native SDK sends media keys using MSC4153's identity-based
        // strategy. Without a verified own cross-signing identity and a signed
        // current device, joining can appear to succeed while peers receive no
        // media key. Report that requirement before opening the SFU session.
        let user_id = self.client.user_id().ok_or(RtcError::SignedOut)?;
        let encryption = self.client.encryption();
        let identity = encryption
            .get_user_identity(user_id)
            .await
            .map_err(|error| RtcError::EncryptionReadiness(error.to_string()))?;
        if !identity.is_some_and(|identity| identity.is_verified()) {
            return Err(RtcError::EncryptionIdentityNotReady);
        }
        let own_device = encryption
            .get_own_device()
            .await
            .map_err(|error| RtcError::EncryptionReadiness(error.to_string()))?;
        if !own_device.is_some_and(|device| device.is_verified_with_cross_signing()) {
            return Err(RtcError::EncryptionIdentityNotReady);
        }
        Ok(())
    }
}

impl Drop for RtcService {
    fn drop(&mut self) {
        for active in self.calls.values() {
            active.lease.revoke();
        }
    }
}

/// Install rustls's AWS-LC provider before the application's first HTTPS or
/// WSS request. The Matrix SDK and LiveKit graph enables two providers, so
/// rustls cannot select one on its own. Safe to call again if the app already
/// installed a provider.
pub fn install_crypto_provider() {
    // The dependency graph brings both ring and aws-lc-rs to rustls. Pick the
    // same provider as matrix-sdk before either Matrix HTTPS or LiveKit WSS is
    // used, unless the application already installed its own provider.
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

#[cfg(test)]
mod tests {
    use matrix_rtc_call_sdk::StreamState;

    use super::*;

    #[test]
    fn default_slot_matches_pinned_rtc_room_slot() {
        assert_eq!(CallOptions::default().slot_id, "m.call#room");
        assert_eq!(CallOptions::default().interop_mode, InteropMode::Current);
    }

    #[test]
    fn element_matrix_2_0_converts_wire_slot_without_changing_core_slot() {
        let options = CallOptions::element_call_matrix_2_0();
        assert_eq!(options.slot_id, "m.call#room");
        let format = MembershipFormat::from(options.interop_mode);
        assert_eq!(format, MembershipFormat::Sticky2025);
        assert_eq!(format.token_slot_id(&options.slot_id), "m.call#ROOM");
        assert_eq!(
            MembershipFormat::Current.token_slot_id(&options.slot_id),
            "m.call#room"
        );
    }

    #[test]
    fn exposed_sticky_modes_reject_plaintext_without_changing_legacy_policy() {
        for options in [
            CallOptions::default(),
            CallOptions::element_call_matrix_2_0(),
        ] {
            assert!(matches!(
                options.validate_encryption(false),
                Err(RtcError::UnencryptedRoomUnsupported)
            ));
            assert!(options.validate_encryption(true).is_ok());
        }
        assert!(
            CallOptions::room_state_compatibility()
                .validate_encryption(false)
                .is_ok()
        );
    }

    #[test]
    fn legacy_room_state_options_use_the_pre_sticky_room_sentinel() {
        let options = CallOptions::room_state_compatibility();
        assert_eq!(options.interop_mode, InteropMode::RoomState);
        assert_eq!(options.slot_id, "m.call#room");
        assert_eq!(CallOptions::default().slot_id, "m.call#room");
    }

    #[test]
    fn roster_conversion_preserves_membership_and_media_state() {
        let info = ParticipantInfo::from(Participant {
            member_id: "membership-1".into(),
            user_id: "@alice:example.test".into(),
            device_id: Some("DEVICE".into()),
            is_local: false,
            reachable: true,
            streams: vec![
                StreamState {
                    kind: MediaStreamKind::Microphone,
                    muted: true,
                },
                StreamState {
                    kind: MediaStreamKind::ScreenShare,
                    muted: false,
                },
            ],
            hand_raised_at_ms: Some(42),
            joined_at_ms: None,
        });
        assert_eq!(info.member_id, "membership-1");
        assert_eq!(info.user_id, "@alice:example.test");
        assert_eq!(info.device_id.as_deref(), Some("DEVICE"));
        assert!(!info.is_local);
        assert!(info.reachable);
        assert_eq!(info.hand_raised_at_ms, Some(42));
        assert_eq!(
            info.streams,
            vec![
                ParticipantStreamInfo {
                    kind: RtcStreamKind::Microphone,
                    muted: true,
                },
                ParticipantStreamInfo {
                    kind: RtcStreamKind::ScreenShare,
                    muted: false,
                },
            ]
        );
    }
}
