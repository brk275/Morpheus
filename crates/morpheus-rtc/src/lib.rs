//! Native MatrixRTC integration boundary.
//!
//! This crate has no WebView path. With `native-rtc`, it wraps Element's
//! MatrixRTC + LiveKit/libwebrtc stack. Without that feature, callers can show
//! a precise unsupported state instead of offering a non-functional call UI.
//! With device features, native microphone/speaker, camera, and display
//! adapters produce/consume those frames. The application owns permissions,
//! device selection, call controls, and remote video rendering.

use serde_json::Value;
use url::Url;

// Exercise the same pure inventory contract used by the Linux build script.
#[cfg(all(test, target_os = "linux", feature = "native-rtc"))]
#[allow(dead_code)]
#[path = "../../../vendor/webrtc-sys/pipewire_isolation.rs"]
mod pipewire_archive_contract;

mod reconnect;
mod voice_space;

#[cfg(feature = "device-audio")]
mod audio_device;
#[cfg(feature = "device-audio")]
mod ring_audio;
#[cfg(feature = "device-audio")]
pub use ring_audio::{
    PcmPlayback, RingAudioAuthority, RingAudioError, RingAudioReleaseError, RingPlayback,
    wait_for_ring_audio_release,
};
#[cfg(all(feature = "device-screen", target_os = "linux"))]
mod linux_portal;
#[cfg(feature = "native-rtc")]
mod media;
#[cfg(feature = "native-rtc")]
mod native;
#[cfg(feature = "native-rtc")]
mod observer;
#[cfg(feature = "native-rtc")]
mod outgoing_notification;
#[cfg(feature = "native-rtc")]
mod outgoing_preflight;
#[cfg(feature = "native-rtc")]
pub mod ringing;
#[cfg(any(feature = "device-camera", feature = "device-screen"))]
mod video_device;

#[cfg(feature = "device-audio")]
pub use audio_device::{
    AudioControls, AudioDevice, AudioDeviceCatalog, AudioDeviceReleaseError, AudioDeviceSelection,
    AudioOutput, AudioResume, AudioRoute, AudioState, DeviceRoute, MicrophoneCapture, RouteNotice,
    audio_devices, wait_for_audio_device_release,
};
#[cfg(feature = "native-rtc")]
pub use matrix_rtc_call_sdk::CallEvent as RtcCallEvent;
#[cfg(feature = "native-rtc")]
pub use matrix_rtc_transport::{
    AudioFrame, I420Buffer, MediaStreamKind, VideoFrame, VideoRotation,
};
// Receive-side presentation: frame-stream handles and per-stream
// subscription constraints (pause or size cap) for a native call stage.
#[cfg(feature = "native-rtc")]
pub use matrix_rtc_transport::{Dimensions, MediaConstraints, RemoteTrackHandle, VideoDetail};
#[cfg(feature = "native-rtc")]
pub use media::{MediaError, Publication};
#[cfg(feature = "native-rtc")]
pub use native::{
    CallInfo, CallOptions, InteropMode, MediaFailureSnapshot, ParticipantInfo,
    ParticipantStreamInfo, RtcService, RtcStreamKind, install_crypto_provider,
};
#[cfg(feature = "native-rtc")]
pub use observer::{ObservedCall, ObservedParticipant, ObserverError, PassiveCallObserver};
#[cfg(feature = "native-rtc")]
pub use outgoing_notification::NotificationDialect;
#[cfg(feature = "native-rtc")]
pub use outgoing_preflight::OutgoingCallError;
pub use reconnect::{
    ReconnectBackoff, ReconnectPolicy, ReconnectWindow, RtcCallEnd, RtcConnectionSignal,
};
#[cfg(feature = "device-camera")]
pub use video_device::CameraCapture;
#[cfg(feature = "device-screen")]
pub use video_device::ScreenCapture;
pub use voice_space::{
    VOICE_SPACE_EVENT_TYPE, VOICE_SPACE_STATE_KEY, VoiceSpaceMarker, VoiceSpaceMarkerError,
    parse_voice_space_state_event,
};

/// What the binary can provide before a homeserver is contacted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalRtcSupport {
    /// Native signalling, E2EE, and LiveKit media frames were compiled in.
    NativeFrames,
    /// The binary was built without the `native-rtc` feature.
    Disabled,
}

pub const fn local_support() -> LocalRtcSupport {
    if cfg!(feature = "native-rtc") {
        LocalRtcSupport::NativeFrames
    } else {
        LocalRtcSupport::Disabled
    }
}

/// Compile-time OS device adapters. An adapter may still fail to open at
/// runtime because a device is absent, busy, or denied by OS permissions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalDeviceSupport {
    pub audio: bool,
    pub camera: bool,
    pub screen: bool,
}

pub const fn local_device_support() -> LocalDeviceSupport {
    LocalDeviceSupport {
        audio: cfg!(feature = "device-audio"),
        camera: cfg!(feature = "device-camera"),
        screen: cfg!(feature = "device-screen"),
    }
}

/// The advertised transport that the native call stack can use.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransportSupport {
    LiveKit { service_url: String },
    NoLiveKitTransport,
}

impl TransportSupport {
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::LiveKit { .. })
    }

    pub fn livekit_service_url(&self) -> Option<&str> {
        match self {
            Self::LiveKit { service_url } => Some(service_url),
            Self::NoLiveKitTransport => None,
        }
    }
}

/// Inspect the `rtc_transports` array returned by Matrix SDK discovery.
///
/// The SDK tries the MSC4143 endpoint and its `rtc_foci` well-known fallback.
/// The first valid LiveKit transport wins, preserving server preference order.
/// Unknown transports do not imply that this client can join them.
pub fn classify_transports(transports: &Value) -> TransportSupport {
    let Some(entries) = transports.as_array() else {
        return TransportSupport::NoLiveKitTransport;
    };

    entries
        .iter()
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("livekit"))
        .filter(|entry| entry.get("url").is_none())
        .filter_map(|entry| entry.get("livekit_service_url").and_then(Value::as_str))
        .find_map(valid_service_url)
        .map(|service_url| TransportSupport::LiveKit { service_url })
        .unwrap_or(TransportSupport::NoLiveKitTransport)
}

// This early discovery filter matches the vendored transport's policy. The
// transport repeats validation for remote membership foci before any OpenID
// request; discovery alone is not a credential security boundary.
fn valid_service_url(raw: &str) -> Option<String> {
    if raw.chars().any(char::is_control) {
        return None;
    }
    let mut url = Url::parse(raw).ok()?;
    if !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    if url.host_str() == Some("localhost") {
        url.set_host(Some("127.0.0.1")).ok()?;
    }
    let loopback = match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        _ => false,
    };
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return None;
    }
    // Keep server path spelling and avoid introducing a trailing slash solely
    // through parsing, except when localhost normalization was necessary.
    if Url::parse(raw).ok()?.host_str() == Some("localhost") {
        Some(url.into())
    } else {
        Some(raw.to_owned())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RtcError {
    #[error("this build has no native MatrixRTC backend")]
    BackendDisabled,
    #[error("the account is not logged in")]
    SignedOut,
    #[error("the room ID is invalid")]
    InvalidRoomId,
    #[error("the room is not loaded in this account")]
    RoomNotLoaded,
    #[error("could not read or write Matrix room state: {0}")]
    RoomState(String),
    #[error("invalid Morpheus voice-space marker: {0}")]
    VoiceSpaceMarker(#[from] VoiceSpaceMarkerError),
    #[error("a call is already active in this room")]
    AlreadyJoined,
    #[error("there is no active call in this room")]
    NotJoined,
    #[error("the call stopped because participant identities could not be uniquely verified")]
    IdentityRejected,
    #[error("the homeserver advertises no usable LiveKit MatrixRTC transport")]
    NoLiveKitTransport,
    #[error("the homeserver does not advertise sticky events required by this call mode")]
    StickyEventsUnsupported,
    #[error("MatrixRTC transport discovery failed: {0}")]
    Discovery(String),
    #[cfg(feature = "native-rtc")]
    #[error("could not check encrypted-call device readiness: {0}")]
    EncryptionReadiness(String),
    #[cfg(feature = "native-rtc")]
    #[error("room encryption state is unknown; retry after room state loads")]
    UnknownEncryptionState,
    #[cfg(feature = "native-rtc")]
    #[error("Standard and Element Call Matrix 2.0 calls require an encrypted room")]
    UnencryptedRoomUnsupported,
    #[cfg(feature = "native-rtc")]
    #[error(
        "this device is not ready for encrypted calls; complete cross-signing verification or recovery"
    )]
    EncryptionIdentityNotReady,
    #[cfg(feature = "native-rtc")]
    #[error("{0}")]
    OutgoingPreflight(#[from] OutgoingCallError),
    #[cfg(feature = "native-rtc")]
    #[error("MatrixRTC call failed: {0}")]
    Call(matrix_rtc_call_sdk::LiveKitCallError),
}

#[cfg(feature = "native-rtc")]
impl From<matrix_rtc_call_sdk::LiveKitCallError> for RtcError {
    fn from(error: matrix_rtc_call_sdk::LiveKitCallError) -> Self {
        match error {
            matrix_rtc_call_sdk::LiveKitCallError::Attach(
                matrix_rtc_call_sdk::AttachError::IdentityRejected,
            ) => Self::IdentityRejected,
            other => Self::Call(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[cfg(feature = "native-rtc")]
    #[test]
    fn identity_refusal_is_not_the_retryable_generic_call_error() {
        let error = matrix_rtc_call_sdk::LiveKitCallError::Attach(
            matrix_rtc_call_sdk::AttachError::IdentityRejected,
        );
        assert!(matches!(RtcError::from(error), RtcError::IdentityRejected));
    }

    #[test]
    fn picks_first_valid_livekit_transport_in_server_order() {
        let transports = json!([
            {"type": "org.example.p2p", "url": "https://p2p.example"},
            {"type": "livekit", "livekit_service_url": "not a URL"},
            {"type": "livekit", "livekit_service_url": "https://first.example/rtc"},
            {"type": "livekit", "livekit_service_url": "https://second.example/rtc"}
        ]);
        assert_eq!(
            classify_transports(&transports),
            TransportSupport::LiveKit {
                service_url: "https://first.example/rtc".into(),
            }
        );
    }

    #[test]
    fn unsupported_and_malformed_advertisements_are_not_call_capabilities() {
        for transports in [
            json!([]),
            json!({"rtc_transports": []}),
            json!([{"type": "org.example.p2p", "url": "https://p2p.example"}]),
            json!([{"type": "livekit", "url": "https://sfu.example.org"}]),
            json!([{"type": "livekit", "url": "https://sfu.example.org", "livekit_service_url": "https://auth.example.org"}]),
            json!([{"type": "livekit", "livekit_service_url": "file:///tmp/token"}]),
            json!([{"type": "livekit", "livekit_service_url": "https://"}]),
            json!([{"type": "livekit", "livekit_service_url": "http://rtc.example.org"}]),
            json!([{"type": "livekit", "livekit_service_url": "https://user:secret@rtc.example.org"}]),
            json!([{"type": "livekit", "livekit_service_url": "https://rtc.example.org?token=secret"}]),
            json!([{"type": "livekit", "livekit_service_url": "https://rtc.example.org#secret"}]),
        ] {
            assert_eq!(
                classify_transports(&transports),
                TransportSupport::NoLiveKitTransport
            );
        }
    }

    #[test]
    fn build_capability_matches_feature() {
        assert_eq!(
            local_support(),
            if cfg!(feature = "native-rtc") {
                LocalRtcSupport::NativeFrames
            } else {
                LocalRtcSupport::Disabled
            }
        );
    }

    #[test]
    fn device_capability_matches_features() {
        assert_eq!(local_device_support().audio, cfg!(feature = "device-audio"));
        assert_eq!(
            local_device_support().camera,
            cfg!(feature = "device-camera")
        );
        assert_eq!(
            local_device_support().screen,
            cfg!(feature = "device-screen")
        );
    }
}

#[cfg(all(target_os = "macos", feature = "device-audio"))]
mod coreaudio_close;
#[cfg(all(target_os = "macos", feature = "device-audio"))]
pub use coreaudio_close::{
    CoreAudioCloseError, close_retired_coreaudio_on_owned_worker, wait_for_all_coreaudio_release,
    wait_for_retired_coreaudio_release,
};

#[cfg(all(target_os = "linux", feature = "device-audio"))]
mod alsa_close;
#[cfg(all(target_os = "linux", feature = "device-audio"))]
pub use alsa_close::{
    AlsaBackendCloseError, close_retired_alsa_on_owned_worker, wait_for_all_alsa_release,
    wait_for_retired_alsa_release,
};
#[cfg(all(windows, feature = "device-audio"))]
pub use cpal::{WasapiCloseError, WasapiCloseObservation, WasapiUiContext, begin_wasapi_shutdown};
