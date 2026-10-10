use std::collections::HashMap;

use matrix_rtc_call_sdk::{LiveKitCall, LiveKitCallOptions, close_slot, open_slot};
use matrix_rtc_core::{LiveKitTransport, SlotEncryption, compat::MembershipFormat};
use matrix_sdk::{Client, ruma::RoomId};

use crate::{RtcError, TransportSupport, classify_transports};

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
            slot_id: "m.call#room".to_owned(),
            interop_mode: InteropMode::Current,
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
    calls: HashMap<String, LiveKitCall>,
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

    /// Publish `m.rtc.slot` before starting a new call. This requires room
    /// state-event power. For encrypted rooms, use `encrypted = true` to select
    /// MatrixRTC's `m.per_member` media-key policy.
    pub async fn open_slot(
        &self,
        room_id: &str,
        slot_id: &str,
        encrypted: bool,
    ) -> Result<(), RtcError> {
        self.ensure_loaded_room(room_id)?;
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
        if encryption_state.is_encrypted() {
            self.ensure_encrypted_call_ready().await?;
        }
        let service_url = match self.discover_transport().await? {
            TransportSupport::LiveKit { service_url } => service_url,
            TransportSupport::NoLiveKitTransport => return Err(RtcError::NoLiveKitTransport),
        };

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
        self.calls.insert(room_id.to_owned(), call);
        Ok(info)
    }

    /// Leave a call cleanly. The call is removed even when the Matrix leave
    /// send fails, because the upstream `leave` consumes its call handle.
    pub async fn leave(&mut self, room_id: &str) -> Result<(), RtcError> {
        let call = self.calls.remove(room_id).ok_or(RtcError::NotJoined)?;
        call.leave().await?;
        Ok(())
    }

    pub fn call(&self, room_id: &str) -> Option<&LiveKitCall> {
        self.calls.get(room_id)
    }

    pub fn call_mut(&mut self, room_id: &str) -> Option<&mut LiveKitCall> {
        self.calls.get_mut(room_id)
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
