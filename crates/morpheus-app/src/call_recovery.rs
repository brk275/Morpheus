//! Interruption policy for the native call worker.
//!
//! Matrix sync carries MatrixRTC memberships and to-device media keys. While
//! it reconnects this device cannot see a member leave, so it cannot rotate
//! its media key away from them. The worker therefore pauses outgoing audio
//! and video (devices stay open; nothing is reacquired) for a bounded grace
//! period and resumes the user's own mute and capture choices afterwards.
//! LiveKit resumes media-transport drops inside one join by itself. Only when
//! the join's own focus is gone, or sync stayed away long enough for the
//! membership's delayed leave to fire, does the worker leave that join and
//! try a bounded number of fresh joins (see `morpheus_rtc::ReconnectPolicy`).

use std::time::Duration;

use morpheus_matrix::{SyncBackend, SyncStatus};
use morpheus_rtc::RtcError;

use crate::calls::CallControllerError;

/// Below the pinned MatrixRTC core's 30-second delayed-leave timeout: past it
/// the homeserver may already have published this device's departure, and
/// peers rotate their keys away from the old membership.
pub(crate) const SIGNALING_GRACE: Duration = Duration::from_secs(25);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SignalingHealth {
    /// Sticky sliding sync is running for the selected account.
    Ready,
    /// Sync is retrying or restarting. Outgoing media must pause.
    Interrupted,
    /// Sync stopped (sign-out, account switch, shutdown) or fell back to a
    /// backend without sticky events. Capture authorization ends.
    Unavailable,
}

pub(crate) fn signaling_health(sync: &SyncStatus) -> SignalingHealth {
    match sync {
        SyncStatus::Running {
            backend: SyncBackend::SlidingSticky,
        } => SignalingHealth::Ready,
        SyncStatus::Retrying { .. } | SyncStatus::Starting => SignalingHealth::Interrupted,
        SyncStatus::Stopped
        | SyncStatus::Running {
            backend: SyncBackend::Classic,
        } => SignalingHealth::Unavailable,
    }
}

/// The temporary gate read at every outgoing audio and video push. It does
/// not depend on the call worker being polled, so a worker suspended in a
/// publish still cannot send while sync is down.
pub(crate) fn transmission_allowed(sync: &SyncStatus) -> bool {
    signaling_health(sync) == SignalingHealth::Ready
}

/// Why a join became unusable while the user still wants the call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RejoinCause {
    /// The SFU connection this device publishes on closed and LiveKit gave
    /// up reconnecting it.
    TransportClosed,
    /// The call session ended without the slot closing, for example because
    /// this membership's delayed leave fired while the device was offline.
    MembershipEnded,
    /// Sync did not return within `SIGNALING_GRACE`.
    SignalingTimeout,
}

impl RejoinCause {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::TransportClosed => "its media connection closed",
            Self::MembershipEnded => "its call membership ended unexpectedly",
            Self::SignalingTimeout => "the homeserver stayed unreachable for too long",
        }
    }
}

/// Fresh-join failures that a later attempt can plausibly clear. Encryption,
/// room and account preconditions are not retried: they need the user.
pub(crate) fn rejoin_retryable(error: &CallControllerError) -> bool {
    match error {
        CallControllerError::TimedOut(_) => true,
        CallControllerError::Rtc(error) => matches!(
            error,
            RtcError::NoLiveKitTransport
                | RtcError::Discovery(_)
                | RtcError::EncryptionReadiness(_)
                | RtcError::UnknownEncryptionState
                | RtcError::Call(_)
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_sticky_sync_loss_ends_capture_authorization() {
        assert_eq!(
            signaling_health(&SyncStatus::Running {
                backend: SyncBackend::SlidingSticky,
            }),
            SignalingHealth::Ready
        );
        for transient in [
            SyncStatus::Starting,
            SyncStatus::Retrying {
                after_seconds: 0,
                reason: "sliding sync is offline".into(),
            },
        ] {
            assert_eq!(signaling_health(&transient), SignalingHealth::Interrupted);
        }
        for terminal in [
            SyncStatus::Stopped,
            SyncStatus::Running {
                backend: SyncBackend::Classic,
            },
        ] {
            assert_eq!(signaling_health(&terminal), SignalingHealth::Unavailable);
        }
    }

    #[test]
    fn only_running_sticky_sync_may_transmit() {
        assert!(transmission_allowed(&SyncStatus::Running {
            backend: SyncBackend::SlidingSticky,
        }));
        for closed in [
            SyncStatus::Starting,
            SyncStatus::Retrying {
                after_seconds: 4,
                reason: "offline".into(),
            },
            SyncStatus::Stopped,
            SyncStatus::Running {
                backend: SyncBackend::Classic,
            },
        ] {
            assert!(!transmission_allowed(&closed));
        }
    }

    #[test]
    fn rejoins_retry_transport_failures_but_not_trust_or_account_failures() {
        for retryable in [
            CallControllerError::TimedOut("rejoining MatrixRTC call"),
            CallControllerError::Rtc(RtcError::Discovery("offline".into())),
            CallControllerError::Rtc(RtcError::NoLiveKitTransport),
        ] {
            assert!(rejoin_retryable(&retryable), "{retryable}");
        }
        for terminal in [
            CallControllerError::Rtc(RtcError::EncryptionIdentityNotReady),
            CallControllerError::Rtc(RtcError::UnencryptedRoomUnsupported),
            CallControllerError::Rtc(RtcError::StickyEventsUnsupported),
            CallControllerError::Rtc(RtcError::RoomNotLoaded),
            CallControllerError::AccountChanged,
            CallControllerError::Cleanup("leave failed".into()),
        ] {
            assert!(!rejoin_retryable(&terminal), "{terminal}");
        }
    }
}
