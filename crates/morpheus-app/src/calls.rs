//! App-facing native voice calls.
//!
//! The `native-calls` app feature includes this module and the `native-devices`
//! RTC feature. The app constructs one controller for its `AccountManager` and
//! invokes its async methods on Tokio. The account manager must keep
//! Sliding Sync with MSC4354 sticky events running throughout a call.
//! Call `morpheus_rtc::install_crypto_provider()` at process startup, before
//! the first Matrix SDK HTTPS/WSS request, so rustls has one chosen provider.
//! A packaged macOS app needs `NSMicrophoneUsageDescription`; microphone
//! permission is requested only when the user starts audio, not by the probe.
//!
//! Before calling `AccountManager::switch_to` or logging out, consult
//! `CallStatus::blocks_account_switch()` and finish `end_call()` first. The
//! controller also notices unexpected account changes and leaves the old
//! call, but the old account's sync may already have stopped by then.
//! A failed leave keeps the switch blocked until the call worker has stopped
//! and the user explicitly acknowledges an unconfirmed server departure.
//! Upstream consumes the call handle during leave, so it cannot be retried.
//!
//! `open_slot()` writes `m.rtc.slot` only on an explicit user action in the
//! standard call mode. Element Call's legacy room-state mode has no slot. It reads
//! the room's current encryption state and never uses Morpheus's optional
//! voice-space presentation marker to decide permissions or call semantics.
//! Live microphone, camera, and screen controls are bound to the current join.
//!
//! Interruptions keep the call (and the account-switch block) alive while it
//! recovers: LiveKit resumes media inside one join, a transient sync loss
//! pauses outgoing media for a bounded grace period, and a lost join is left
//! and rejoined a bounded number of times with the same options. See
//! `call_recovery`. Leave, account changes and terminal sync loss always win.

use std::{
    fmt,
    future::{Future, pending},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use morpheus_matrix::{
    AccountInfo, AccountManager, AccountSelection, MatrixCoreError, SyncBackend, SyncStatus,
};
pub use morpheus_rtc::CallInfo;
use morpheus_rtc::{
    AudioControls, AudioDeviceSelection, AudioResume, AudioState, CallOptions, LocalRtcSupport,
    MediaError, ReconnectBackoff, ReconnectPolicy, ReconnectWindow, RtcCallEnd,
    RtcConnectionSignal, RtcError, RtcService, TransportSupport, local_device_support,
    local_support,
};
pub use morpheus_rtc::{AudioRoute, DeviceRoute, RouteNotice};
use tokio::{
    sync::{Mutex, broadcast, mpsc, oneshot, watch},
    time::timeout,
};

use crate::{
    call_recovery::{
        RejoinCause, SIGNALING_GRACE, SignalingHealth, rejoin_retryable, signaling_health,
        transmission_allowed,
    },
    incoming_ring::controller::{AcceptedOffer, EstablishedAuthorization, EstablishedGuard},
    local_video::{
        CaptureCommand, CaptureKind, CaptureSource, CaptureStop, LocalVideoSession,
        LocalVideoStatus,
    },
    outgoing_ring::{InvitationOutcome, OutgoingRing},
    updater::{CallLease as UpdateCallLease, UpdateActivity, UpdateError},
    video_render::{RosterUpdate, StageRequest, VideoUpdate, run_remote_video},
    voice_spaces::VoiceSpaces,
};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const JOIN_TIMEOUT: Duration = Duration::from_secs(45);
const LEAVE_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(75);
/// How long "Use for this call" may wait for the account manager and the call
/// slot. Past it, nothing is queued and nothing is retried.
const ROUTE_ADMISSION_TIMEOUT: Duration = Duration::from_secs(5);
const INVITATION_INTERRUPTED: &str = "Invitation interrupted by a lost connection.";

/// A read-only compatibility result. `TransportReady` means the binary,
/// account sync, and homeserver transport can support native calls; it does
/// not test OS permissions, devices, or whether a room has an open slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioCallCapability {
    NativeRtcNotBuilt,
    AudioBackendNotBuilt,
    SignInRequired,
    StickySyncRequired(SyncStatus),
    NoLiveKitTransport,
    TransportReady { livekit_service_url: String },
}

/// Choose one MatrixRTC format before joining; no runtime protocol fallback.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CallJoinMode {
    #[default]
    Standard,
    ElementCallMatrix2,
    ElementCallRoomState,
}

/// Joining a voice space or an existing call is quiet. Notifications require
/// the separate explicit action captured before any asynchronous work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallStartIntent {
    Quiet,
    Notify,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InvitationUpdate {
    pub token: u64,
    pub text: &'static str,
}

impl CallJoinMode {
    fn options(self) -> CallOptions {
        match self {
            Self::Standard => CallOptions::default(),
            Self::ElementCallMatrix2 => CallOptions::element_call_matrix_2_0(),
            Self::ElementCallRoomState => CallOptions::room_state_compatibility(),
        }
    }
}

impl From<morpheus_config::RoomCallMode> for CallJoinMode {
    fn from(mode: morpheus_config::RoomCallMode) -> Self {
        match mode {
            morpheus_config::RoomCallMode::Standard => Self::Standard,
            morpheus_config::RoomCallMode::ElementCallMatrix2 => Self::ElementCallMatrix2,
            morpheus_config::RoomCallMode::ElementCallLegacy => Self::ElementCallRoomState,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallStatus {
    Idle,
    OpeningSlot {
        account_id: String,
        room_id: String,
    },
    Starting {
        account_id: String,
        room_id: String,
    },
    Joined {
        account_id: String,
        call: CallInfo,
    },
    /// Still the user's call, recovering from an interruption. `call` is the
    /// latest join, which may already have been left while rejoining.
    Reconnecting {
        account_id: String,
        call: CallInfo,
        interruption: CallInterruption,
    },
    Ending {
        account_id: String,
        room_id: String,
    },
    CleanupFailed {
        account_id: String,
        room_id: String,
        reason: String,
    },
    Failed {
        room_id: String,
        reason: String,
    },
    /// The call ended without a local Leave, e.g. its slot was closed.
    Ended {
        room_id: String,
        reason: String,
    },
}

impl CallStatus {
    /// The UI should wait for `end_call()` before switching or logging out.
    pub fn blocks_account_switch(&self) -> bool {
        matches!(
            self,
            Self::OpeningSlot { .. }
                | Self::Starting { .. }
                | Self::Joined { .. }
                | Self::Reconnecting { .. }
                | Self::Ending { .. }
                | Self::CleanupFailed { .. }
        )
    }

    /// Joined or recovering, so the call's media controls are offered.
    /// While a lost join is replaced, capture can be stopped but not started.
    pub fn controls_available(&self) -> bool {
        matches!(self, Self::Joined { .. } | Self::Reconnecting { .. })
    }
}

/// What a joined call is recovering from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallInterruption {
    /// LiveKit is resuming the SFU media connection inside the same join.
    Media,
    /// Matrix sync is reconnecting. Outgoing audio and video are paused,
    /// because a member leaving now could not trigger a media-key rotation.
    Signaling,
    /// The join was lost; waiting for the homeserver before leaving it and
    /// joining again.
    WaitingForNetwork,
    /// The join was lost; a fresh join with the same options is under way.
    Rejoining { attempt: u32, max_attempts: u32 },
}

impl CallInterruption {
    pub fn label(self) -> String {
        match self {
            Self::Media => "Reconnecting call media…".into(),
            Self::Signaling => {
                "Reconnecting to the homeserver… microphone and video are paused".into()
            }
            Self::WaitingForNetwork => "Connection lost · waiting for the network…".into(),
            Self::Rejoining {
                attempt,
                max_attempts,
            } => format!("Connection lost · rejoining (attempt {attempt} of {max_attempts})…"),
        }
    }
}

#[derive(Debug)]
pub enum CallControllerError {
    Unavailable(AudioCallCapability),
    AlreadyBusy,
    Cancelled,
    AccountChanged,
    StaleCall,
    IncomingOfferUnavailable,
    RoomNotLoaded,
    UnknownEncryptionState,
    RoomState(String),
    Matrix(MatrixCoreError),
    Rtc(RtcError),
    Media(MediaError),
    WorkerStopped,
    TimedOut(&'static str),
    Cleanup(String),
    Update(UpdateError),
    ConnectionLost(String),
}

impl fmt::Display for CallControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(capability) => {
                write!(formatter, "native audio unavailable: {capability:?}")
            }
            Self::AlreadyBusy => write!(formatter, "a call is already starting or active"),
            Self::Cancelled => write!(formatter, "call start was cancelled"),
            Self::AccountChanged => write!(formatter, "the active Matrix account changed"),
            Self::StaleCall => write!(formatter, "that call is no longer active"),
            Self::IncomingOfferUnavailable => {
                write!(formatter, "that incoming call is no longer available")
            }
            Self::RoomNotLoaded => {
                write!(formatter, "the room is not loaded in the active account")
            }
            Self::UnknownEncryptionState => {
                write!(formatter, "the room's encryption state is not known yet")
            }
            Self::RoomState(error) => {
                write!(formatter, "could not read room encryption state: {error}")
            }
            Self::Matrix(error) => write!(formatter, "Matrix account error: {error}"),
            Self::Rtc(error) => write!(formatter, "MatrixRTC error: {error}"),
            Self::Media(error) => write!(formatter, "native media error: {error}"),
            Self::WorkerStopped => write!(formatter, "the call worker stopped unexpectedly"),
            Self::TimedOut(phase) => write!(formatter, "timed out while {phase}"),
            Self::Cleanup(error) => write!(formatter, "could not leave the call cleanly: {error}"),
            Self::Update(error) => write!(formatter, "{error}"),
            Self::ConnectionLost(detail) => {
                write!(formatter, "the call connection was lost: {detail}")
            }
        }
    }
}

impl std::error::Error for CallControllerError {}

impl From<MatrixCoreError> for CallControllerError {
    fn from(value: MatrixCoreError) -> Self {
        Self::Matrix(value)
    }
}

impl From<RtcError> for CallControllerError {
    fn from(value: RtcError) -> Self {
        Self::Rtc(value)
    }
}

impl From<MediaError> for CallControllerError {
    fn from(value: MediaError) -> Self {
        Self::Media(value)
    }
}

struct RunningCall {
    token: u64,
    /// UI generation of the current join. Equal to `token` until a rejoin
    /// starts a new one, so the ended join's queued UI updates are rejected.
    presentation: u64,
    account_id: String,
    room_id: String,
    cancel: Option<oneshot::Sender<()>>,
    done: watch::Receiver<Option<Result<(), String>>>,
    audio_controls: Option<AudioControls>,
    video_commands: mpsc::Sender<CaptureCommand>,
    capture_stop: Arc<CaptureStop>,
}

/// One call as a control observed it, read together with its status
/// (`CallController::observe`): its account and room, the call's own
/// lifecycle generation (`call`: `RunningCall::token`, unchanged by rejoins)
/// and the media generation of the join that was shown (`presentation`:
/// `RunningCall::presentation`, replaced by each rejoin). A control acting on
/// it later is admitted only under the slot's lock, only for exactly that
/// call (and, for its media, exactly that join); another call never matches,
/// whatever its account and room.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedCall {
    pub account_id: String,
    pub room_id: String,
    pub call: u64,
    pub presentation: u64,
}

impl RunningCall {
    /// Exactly the observed call, through any of its joins, not yet leaving.
    fn is_call(&self, observed: &ObservedCall) -> bool {
        observed.call != 0
            && self.token == observed.call
            && self.account_id == observed.account_id
            && self.room_id == observed.room_id
            && self.cancel.is_some()
    }

    /// Exactly the observed call's shown join, not yet leaving or stopping
    /// its capture.
    fn is_join(&self, observed: &ObservedCall) -> bool {
        self.is_call(observed)
            && observed.presentation != 0
            && self.presentation == observed.presentation
            && !self.capture_stop.requested()
    }
}

/// An applied audio state is scoped to one call generation. A control click
/// made for an earlier generation cannot change a later join.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioUpdate {
    Unavailable {
        token: u64,
    },
    /// Local capture/playback failed, while the MatrixRTC call stays joined
    /// so remote participants and video remain visible.
    Failed {
        token: u64,
        reason: AudioFailure,
    },
    Applied {
        token: u64,
        state: AudioState,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioFailure {
    MicrophoneUnavailable,
    SpeakerUnavailable,
    Other,
}

impl AudioFailure {
    fn from_error(error: &MediaError) -> Self {
        match error {
            MediaError::MicrophoneUnavailable => Self::MicrophoneUnavailable,
            MediaError::SpeakerUnavailable => Self::SpeakerUnavailable,
            _ => Self::Other,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::MicrophoneUnavailable => {
                "Microphone unavailable or ambiguous. Choose another in Settings under Call audio devices, then Use for this call."
            }
            Self::SpeakerUnavailable => {
                "Speaker unavailable or ambiguous. Choose another in Settings under Call audio devices, then Use for this call."
            }
            Self::Other => {
                "Local audio could not start or stopped. Check device permissions, then choose devices in Settings and Use for this call."
            }
        }
    }
}

impl AudioUpdate {
    pub fn token(self) -> u64 {
        match self {
            Self::Unavailable { token }
            | Self::Failed { token, .. }
            | Self::Applied { token, .. } => token,
        }
    }
}

/// The local audio route of call generation `token`, as its audio loop last
/// applied it. Device names in it are for showing only.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AudioRouteUpdate {
    pub token: u64,
    pub route: AudioRoute,
}

/// A route the user chose for call generation `token` (`set_audio_route`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct RouteRequest {
    token: u64,
    selection: AudioDeviceSelection,
    /// The same selection as the route this generation runs with
    /// (`Shared::route_baseline`): the user asked again with nothing changed.
    /// Decided at admission, one request at a time, so coalesced changes
    /// never look like a repeat.
    retry: bool,
}

/// One line for the call page: the device each direction of call audio uses.
/// Platform names are bounded and stripped of control and direction
/// characters before they are shown.
pub fn route_summary(route: &AudioRoute) -> String {
    fn side(label: &str, route: &DeviceRoute) -> String {
        let name = |name: &Option<String>| {
            name.as_deref()
                .map(crate::audio_devices::friendly_name)
                .filter(|name| !name.is_empty())
        };
        match route {
            DeviceRoute::Opening => format!("{label}: opening…"),
            DeviceRoute::Chosen {
                name: device,
                system_default: true,
            } => match name(device) {
                Some(device) => format!("{label}: System default ({device})"),
                None => format!("{label}: System default"),
            },
            DeviceRoute::Chosen { name: device, .. } => format!(
                "{label}: {}",
                name(device).unwrap_or_else(|| "the chosen device".to_owned())
            ),
            DeviceRoute::Previous { name: device } => format!(
                "{label}: {} (the chosen one could not be opened)",
                name(device).unwrap_or_else(|| "the device in use before".to_owned())
            ),
            DeviceRoute::DefaultInstead { name: device } => match name(device) {
                Some(device) => format!("{label}: System default ({device}) instead"),
                None => format!("{label}: System default instead"),
            },
            DeviceRoute::Lost => format!("{label}: none in use"),
        }
    }
    format!(
        "{} · {}",
        side("Microphone", &route.microphone),
        side("Speaker", &route.speaker)
    )
}

/// What changed, in words. The route summary says what holds now.
pub fn route_notice(notice: RouteNotice) -> &'static str {
    match notice {
        RouteNotice::Switched => "Call audio switched to the chosen devices.",
        RouteNotice::MicrophoneSwitchFailed => {
            "The chosen microphone could not be opened. The microphone in use did not change."
        }
        RouteNotice::SpeakerSwitchFailed => {
            "The chosen speaker could not be opened. The speaker in use did not change."
        }
        RouteNotice::MicrophoneLost => {
            "Your microphone was disconnected. Nothing is sent from it; if it has an exact device ID, Morpheus looks for it for up to 10 minutes. Or choose another."
        }
        RouteNotice::MicrophoneDenied => {
            "The system withdrew microphone access. Nothing is sent from the microphone."
        }
        RouteNotice::MicrophoneRestored => {
            "The chosen microphone is open and in use. Your mute setting did not change."
        }
        RouteNotice::SpeakerFellBack => {
            "Your speaker was disconnected. Call audio plays through the system default output until it returns."
        }
        RouteNotice::SpeakerLost => {
            "Your speaker was disconnected and no output is in use, so call audio is not playing."
        }
        RouteNotice::SpeakerRestored => "Your speaker is back and in use again.",
        RouteNotice::DefaultSpeakerReopened => {
            "Your speaker was disconnected. Call audio now plays through the system default output."
        }
        RouteNotice::ReconnectStopped => {
            "Stopped looking for the disconnected device. Choose devices in Settings and Use for this call."
        }
        RouteNotice::MicrophoneNotResumed => {
            "Call audio started again, but your microphone has no exact device identity to reopen it by, so it was not reopened and nothing is sent from it. Choose a microphone and Use for this call."
        }
    }
}

/// What a "Use for this call" reply may still claim when it arrives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteReplyScope {
    /// The generation it was made for is still on screen: its outcome stands.
    Current,
    /// The same call rejoined under a new generation (an accepted request
    /// carries over to it; a refused one does not).
    Rejoined,
    /// That call ended, whether or not another call has started since.
    Ended,
}

/// Compare the call and generation a click was made for (`clicked_call`,
/// `clicked_generation`) with those on screen now. Only the same call
/// (`CallController::call_token`) under a new generation is a rejoin; a new
/// nonzero generation of another call is not.
pub fn route_reply_scope(
    clicked_generation: u64,
    clicked_call: u64,
    generation_now: u64,
    call_now: u64,
) -> RouteReplyScope {
    if clicked_call == 0 || call_now != clicked_call || generation_now == 0 {
        RouteReplyScope::Ended
    } else if generation_now == clicked_generation {
        RouteReplyScope::Current
    } else {
        RouteReplyScope::Rejoined
    }
}

/// The settled "Use for this call" notice: `outcome` (the admission's own
/// text) while its generation is on screen, else only what is still true.
/// After a rejoin nothing is promised: a carried choice is applied, but a
/// retry is not carried (`Shared::retag_route_for_rejoin`), so the user is
/// asked to check and choose again rather than told the switch will happen.
pub fn route_reply_notice(
    scope: RouteReplyScope,
    accepted: bool,
    outcome: &'static str,
) -> &'static str {
    match scope {
        RouteReplyScope::Current => outcome,
        RouteReplyScope::Ended => "The call ended before the switch was confirmed.",
        RouteReplyScope::Rejoined if accepted => {
            "The call reconnected while switching. The call page shows which devices are in use; if they are not the ones you chose, choose Use for this call again."
        }
        RouteReplyScope::Rejoined => {
            "The call reconnected before the switch was made. Choose Use for this call again."
        }
    }
}

/// Whether either direction is not on the device the user chose, so the call
/// page must not call local audio ready.
pub fn route_degraded(route: &AudioRoute) -> bool {
    [&route.microphone, &route.speaker].into_iter().any(|side| {
        matches!(
            side,
            DeviceRoute::Previous { .. } | DeviceRoute::DefaultInstead { .. } | DeviceRoute::Lost
        )
    })
}

enum CallSlot {
    Idle,
    OpeningSlot {
        token: u64,
        account_id: String,
        room_id: String,
        done: watch::Receiver<bool>,
    },
    Running(RunningCall),
    CleanupFailed {
        token: u64,
        done: watch::Receiver<Option<Result<(), String>>>,
    },
}

/// Synchronous UI-request veto held across account-transition waits and through
/// queued completion. Its epoch survives a failed/cancelled request, so an old
/// ring cannot resume after a brief request that never reserved a native call.
#[derive(Default)]
struct RingRequestGate {
    pending: AtomicBool,
    epoch: AtomicU64,
}
pub(crate) struct RingRequestLease(Arc<RingRequestGate>);
impl RingRequestGate {
    fn reserve(self: &Arc<Self>) -> Option<RingRequestLease> {
        self.pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        let _ = self
            .epoch
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                Some(n.saturating_add(1))
            });
        Some(RingRequestLease(self.clone()))
    }
    fn allowed(&self, epoch: u64) -> bool {
        epoch != u64::MAX
            && !self.pending.load(Ordering::Acquire)
            && self.epoch.load(Ordering::Acquire) == epoch
    }
}
impl Drop for RingRequestLease {
    fn drop(&mut self) {
        self.0.pending.store(false, Ordering::Release);
    }
}

/// A bounded test observer of actual SDK key-import receipts, never key bytes.
#[cfg(all(test, target_os = "linux"))]
#[derive(Clone)]
pub(crate) struct RecoveryKeyObservation {
    pub bounded: bool,
    pub presentation: u64,
    pub member_id: String,
    pub identity: String,
    pub key_index: u8,
}

struct Shared {
    ring_request: Arc<RingRequestGate>,
    slot: Mutex<CallSlot>,
    update_activity: UpdateActivity,
    // Access while holding `slot`. Keeping this separate from the state enum
    // preserves the same lease through Running -> CleanupFailed transitions.
    // A worker panic retains the lease until explicit completed cleanup.
    update_call_lease: std::sync::Mutex<Option<UpdateCallLease>>,
    status: watch::Sender<CallStatus>,
    invitation: watch::Sender<InvitationUpdate>,
    video: watch::Sender<VideoUpdate>,
    roster: watch::Sender<RosterUpdate>,
    // Call-stage pin/page/visibility requests; see `choose_stage`.
    stage: watch::Sender<StageRequest>,
    audio: watch::Sender<AudioUpdate>,
    /// The audio route the current generation's loop last applied.
    audio_route: watch::Sender<AudioRouteUpdate>,
    /// The route the user last chose for a call generation; the worker reads it.
    route_requests: watch::Sender<RouteRequest>,
    /// The route a call generation runs with: what its join opened, then each
    /// request admitted for it. A request equal to it is the user's
    /// unchanged repeat (`RouteRequest::retry`). Written under `slot`.
    route_baseline: std::sync::Mutex<(u64, AudioDeviceSelection)>,
    local_video: watch::Sender<LocalVideoStatus>,
    active_capture_workers: Arc<AtomicUsize>,
    active_video_token: AtomicU64,
    active_audio_token: AtomicU64,
    /// The running call itself (its first generation's token), unchanged by
    /// rejoins and retired only by that same call; 0 without a call.
    active_call_token: AtomicU64,
    next_token: AtomicU64,
    #[cfg(all(test, target_os = "linux"))]
    recovery_keys: broadcast::Sender<RecoveryKeyObservation>,
}

impl Shared {
    fn reserve_update_lease(&self) -> Result<(), CallControllerError> {
        let lease = self
            .update_activity
            .try_start_call()
            .map_err(CallControllerError::Update)?;
        *self
            .update_call_lease
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(lease);
        Ok(())
    }

    fn release_update_lease(&self) {
        self.update_call_lease
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }

    async fn reserve_open_slot(
        &self,
        account_id: &str,
        room_id: &str,
    ) -> Result<(u64, watch::Sender<bool>), CallControllerError> {
        let mut slot = self.slot.lock().await;
        if !matches!(*slot, CallSlot::Idle) {
            return Err(CallControllerError::AlreadyBusy);
        }
        self.reserve_update_lease()?;
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let (done_tx, done_rx) = watch::channel(false);
        *slot = CallSlot::OpeningSlot {
            token,
            account_id: account_id.to_owned(),
            room_id: room_id.to_owned(),
            done: done_rx,
        };
        self.status.send_replace(CallStatus::OpeningSlot {
            account_id: account_id.to_owned(),
            room_id: room_id.to_owned(),
        });
        Ok((token, done_tx))
    }

    async fn finish_open_slot(&self, token: u64) -> Result<(), CallControllerError> {
        self.finish_open_slot_after_ring(token, morpheus_rtc::wait_for_ring_audio_release())
            .await
    }

    async fn finish_open_slot_after_ring(
        &self,
        token: u64,
        released: impl Future<Output = Result<(), morpheus_rtc::RingAudioReleaseError>>,
    ) -> Result<(), CallControllerError> {
        let (account_id, room_id) = {
            let slot = self.slot.lock().await;
            match &*slot {
                CallSlot::OpeningSlot {
                    token: current,
                    account_id,
                    room_id,
                    ..
                } if *current == token => (account_id.clone(), room_id.clone()),
                _ => return Ok(()),
            }
        };
        // This network-only operation revoked ringing at its first reservation.
        // Its lease cannot publish a native-clean Idle while that owner remains
        // unjoined, even when the state write itself succeeded or failed.
        let cleanup = released.await.map_err(|error| error.to_string());
        let mut slot = self.slot.lock().await;
        if matches!(&*slot, CallSlot::OpeningSlot { token: current, .. } if *current == token) {
            if let Err(reason) = &cleanup {
                let (_done, done) = watch::channel(Some(Err(reason.clone())));
                *slot = CallSlot::CleanupFailed { token, done };
                self.status.send_replace(CallStatus::CleanupFailed {
                    account_id,
                    room_id,
                    reason: reason.clone(),
                });
            } else {
                *slot = CallSlot::Idle;
                self.release_update_lease();
                self.status.send_replace(CallStatus::Idle);
            }
        }
        cleanup.map_err(CallControllerError::Cleanup)
    }

    /// Publish a live-call status only while the worker for `token` still
    /// owns the un-cancelled slot, so a Leave's `Ending` is never overwritten.
    async fn publish_live_status(&self, token: u64, stop: &CaptureStop, status: CallStatus) {
        let slot = self.slot.lock().await;
        let owned = matches!(
            &*slot,
            CallSlot::Running(running) if running.token == token && running.cancel.is_some()
        );
        if owned && !stop.requested() {
            self.status.send_if_modified(|current| {
                if *current == status {
                    return false;
                }
                *current = status;
                true
            });
        }
    }

    fn send_applied_audio(&self, token: u64, state: AudioState) {
        if self.active_audio_token.load(Ordering::SeqCst) == token {
            self.audio
                .send_replace(AudioUpdate::Applied { token, state });
        }
    }

    fn clear_audio(&self, token: u64) {
        let cleared = self
            .active_audio_token
            .compare_exchange(token, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if cleared || self.active_audio_token.load(Ordering::SeqCst) == 0 {
            self.audio
                .send_replace(AudioUpdate::Unavailable { token: 0 });
        }
    }

    fn fail_audio(&self, token: u64, reason: AudioFailure) {
        if self
            .active_audio_token
            .compare_exchange(token, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.audio
                .send_replace(AudioUpdate::Failed { token: 0, reason });
        }
    }

    /// Audio restarted inside the same generation after it had stopped: show
    /// it as preparing again, under that generation's token. Call with the
    /// call slot held and its generation checked.
    fn rearm_audio(&self, token: u64) {
        let rearmed = self
            .active_audio_token
            .compare_exchange(0, token, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if rearmed || self.active_audio_token.load(Ordering::SeqCst) == token {
            self.audio.send_replace(AudioUpdate::Unavailable { token });
        }
    }

    /// A route report renders only for the current call generation.
    fn send_audio_route(&self, token: u64, route: AudioRoute) {
        if token != 0 && self.active_video_token.load(Ordering::SeqCst) == token {
            self.audio_route
                .send_replace(AudioRouteUpdate { token, route });
        }
    }

    /// Carry generation `previous`'s route choice and baseline to the rejoin
    /// that replaces it (`generation`). Call with the slot held. A retry is
    /// never carried: whether the lost join consumed it is not known, and an
    /// already-consumed System default or alias retry must not reopen another
    /// microphone on the rejoin. The reply for it says so
    /// (`route_reply_notice`).
    fn retag_route_for_rejoin(&self, previous: u64, generation: u64) {
        self.route_requests.send_if_modified(|request| {
            let ours = request.token == previous;
            if ours {
                request.token = generation;
                request.retry = false;
            }
            ours
        });
        let mut baseline = self
            .route_baseline
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if baseline.0 == previous {
            baseline.0 = generation;
        }
    }

    fn clear_media(&self, call: u64, token: u64) {
        if self.active_video_token.load(Ordering::SeqCst) == token {
            self.audio_route.send_replace(AudioRouteUpdate::default());
        }
        self.clear_video(call, token);
        self.clear_audio(token);
        if self.active_video_token.load(Ordering::SeqCst) == 0 {
            self.invitation.send_replace(InvitationUpdate::default());
        }
    }

    fn send_invitation(&self, token: u64, selection: &AccountSelection, text: &'static str) {
        if token != 0
            && self.active_video_token.load(Ordering::SeqCst) == token
            && selection.is_current()
        {
            self.invitation
                .send_replace(InvitationUpdate { token, text });
        }
    }

    /// Retire only the lifecycle identity whose cleanup is completing. The
    /// video clear and this step are separate atomic operations: an older
    /// cleanup may resume here after a replacement call has started.
    fn clear_call_identity(&self, call: u64) {
        let _ =
            self.active_call_token
                .compare_exchange(call, 0, Ordering::SeqCst, Ordering::SeqCst);
    }

    fn clear_video(&self, call: u64, token: u64) {
        let cleared = self
            .active_video_token
            .compare_exchange(token, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if cleared {
            self.clear_call_identity(call);
        }
        // A late producer can race the first clear while its async task is
        // being cancelled. Re-send on final worker cleanup, but never clear a
        // newer call's generation.
        if cleared || self.active_video_token.load(Ordering::SeqCst) == 0 {
            self.video.send_replace(VideoUpdate::Clear { token: 0 });
            self.local_video.send_replace(LocalVideoStatus::default());
            self.roster.send_replace(RosterUpdate {
                token: 0,
                members: Vec::new(),
            });
        }
    }
}

/// Owns at most one native audio call for the selected account. The worker
/// holds the `RtcService` until it leaves, including when media setup fails.
pub struct CallController {
    manager: Arc<AccountManager>,
    shared: Arc<Shared>,
}

impl CallController {
    pub fn new(manager: Arc<AccountManager>, update_activity: UpdateActivity) -> Self {
        let (status, _) = watch::channel(CallStatus::Idle);
        let (video, _) = watch::channel(VideoUpdate::Clear { token: 0 });
        let (roster, _) = watch::channel(RosterUpdate {
            token: 0,
            members: Vec::new(),
        });
        let (audio, _) = watch::channel(AudioUpdate::Unavailable { token: 0 });
        let (local_video, _) = watch::channel(LocalVideoStatus::default());
        Self {
            manager,
            shared: Arc::new(Shared {
                ring_request: Arc::new(RingRequestGate::default()),
                slot: Mutex::new(CallSlot::Idle),
                update_activity,
                update_call_lease: std::sync::Mutex::new(None),
                status,
                invitation: watch::channel(InvitationUpdate::default()).0,
                video,
                roster,
                stage: watch::channel(StageRequest::default()).0,
                audio,
                audio_route: watch::channel(AudioRouteUpdate::default()).0,
                route_requests: watch::channel(RouteRequest::default()).0,
                route_baseline: std::sync::Mutex::default(),
                local_video,
                active_capture_workers: Arc::new(AtomicUsize::new(0)),
                active_video_token: AtomicU64::new(0),
                active_audio_token: AtomicU64::new(0),
                active_call_token: AtomicU64::new(0),
                next_token: AtomicU64::new(1),
                #[cfg(all(test, target_os = "linux"))]
                recovery_keys: broadcast::channel(64).0,
            }),
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn subscribe_recovery_keys(&self) -> broadcast::Receiver<RecoveryKeyObservation> {
        self.shared.recovery_keys.subscribe()
    }

    pub fn subscribe_status(&self) -> watch::Receiver<CallStatus> {
        self.shared.status.subscribe()
    }

    pub fn subscribe_invitation(&self) -> watch::Receiver<InvitationUpdate> {
        self.shared.invitation.subscribe()
    }

    pub fn invitation(&self) -> InvitationUpdate {
        *self.shared.invitation.borrow()
    }

    pub fn subscribe_video(&self) -> watch::Receiver<VideoUpdate> {
        self.shared.video.subscribe()
    }

    pub fn subscribe_roster(&self) -> watch::Receiver<RosterUpdate> {
        self.shared.roster.subscribe()
    }

    /// Pin, unpin or page the call stage only for the call generation the
    /// UI observed. The stage discards another generation's choices.
    pub fn choose_stage(&self, token: u64, change: impl FnOnce(&mut StageRequest)) {
        if token != 0 && self.video_token() == token {
            self.shared
                .stage
                .send_modify(|request| request.choose(token, change));
        }
    }

    /// Whether a call stage is on screen. A hidden stage decodes no video.
    pub fn set_stage_presented(&self, presented: bool) {
        self.shared
            .stage
            .send_if_modified(|request| request.present(presented));
    }

    pub fn subscribe_audio(&self) -> watch::Receiver<AudioUpdate> {
        self.shared.audio.subscribe()
    }

    /// Which device each direction of the current call's audio uses, and the
    /// last route change worth telling the user about.
    pub fn subscribe_audio_route(&self) -> watch::Receiver<AudioRouteUpdate> {
        self.shared.audio_route.subscribe()
    }

    /// Switch the observed call generation's microphone and speaker to
    /// `selection` without leaving. A running audio loop opens the new devices
    /// beside the ones in use and swaps each only once it is open, keeping
    /// the published microphone track, its encryption and the mute and deafen
    /// state; if local audio had stopped (a chosen device was missing at the
    /// start, or another open was still in progress), it starts again on
    /// `selection` inside the same join, publishing a new microphone track
    /// with the stopped controls' mute and deafen state. The outcome arrives on
    /// `subscribe_audio_route()` and `subscribe_audio()`; this only queues it.
    /// Pass only validated IDs from the device catalog. Nothing is persisted
    /// here, and no microphone or camera is enabled that was not.
    pub async fn set_audio_route(
        &self,
        token: u64,
        selection: AudioDeviceSelection,
    ) -> Result<(), CallControllerError> {
        // Fenced on the call generation, not the audio token: a local audio
        // failure clears that, and choosing other devices is how it recovers.
        if token == 0 || self.video_token() != token {
            return Err(CallControllerError::StaleCall);
        }
        let status = self.subscribe_status();
        let admission = async {
            let account = self
                .manager
                .active_account()
                .await
                .ok_or(CallControllerError::AccountChanged)?;
            admit_route_request(&self.shared, token, &account.id, selection).await
        };
        bounded_route_admission(
            &self.shared,
            status,
            token,
            ROUTE_ADMISSION_TIMEOUT,
            admission,
        )
        .await
    }

    pub fn subscribe_local_video(&self) -> watch::Receiver<LocalVideoStatus> {
        self.shared.local_video.subscribe()
    }

    /// A queued UI update may only render for the current call generation.
    pub fn video_token(&self) -> u64 {
        self.shared.active_video_token.load(Ordering::SeqCst)
    }

    /// The running call itself, unchanged when it rejoins under a new
    /// generation (`video_token`); 0 without a call. Each call, whatever its
    /// account or room, gets its own.
    pub fn call_token(&self) -> u64 {
        self.shared.active_call_token.load(Ordering::SeqCst)
    }

    pub fn audio_token(&self) -> u64 {
        self.shared.active_audio_token.load(Ordering::SeqCst)
    }

    /// Changes microphone publication only for the call the button observed.
    /// The UI reads `subscribe_audio()` for the actual applied state.
    pub async fn set_microphone_muted(
        &self,
        token: u64,
        muted: bool,
    ) -> Result<(), CallControllerError> {
        self.with_current_audio_controls(token, |controls| controls.set_microphone_muted(muted))
            .await
    }

    /// Silences this device's speaker only for the observed call join.
    pub async fn set_deafened(
        &self,
        token: u64,
        deafened: bool,
    ) -> Result<(), CallControllerError> {
        self.with_current_audio_controls(token, |controls| controls.set_deafened(deafened))
            .await
    }

    /// Request a native camera or selected window/display publication for the
    /// call generation the UI observed. The watch status reports when frames
    /// are actually being captured and published.
    pub async fn start_video_capture(
        &self,
        token: u64,
        source: CaptureSource,
        label: String,
    ) -> Result<(), CallControllerError> {
        self.send_video_command(token, |reply| CaptureCommand::Start {
            source,
            label,
            reply,
        })
        .await
    }

    pub async fn stop_video_capture(
        &self,
        token: u64,
        kind: CaptureKind,
    ) -> Result<(), CallControllerError> {
        self.send_video_command(token, |reply| CaptureCommand::Stop { kind, reply })
            .await
    }

    async fn send_video_command(
        &self,
        token: u64,
        command: impl FnOnce(oneshot::Sender<Result<(), MediaError>>) -> CaptureCommand,
    ) -> Result<(), CallControllerError> {
        if token == 0 || self.video_token() != token {
            return Err(CallControllerError::StaleCall);
        }
        let account = self
            .manager
            .active_account()
            .await
            .ok_or(CallControllerError::AccountChanged)?;
        let sender = {
            let slot = self.shared.slot.lock().await;
            let CallSlot::Running(running) = &*slot else {
                return Err(CallControllerError::StaleCall);
            };
            if running.presentation != token
                || running.account_id != account.id
                || running.cancel.is_none()
                || running.capture_stop.requested()
                || !self.status().controls_available()
            {
                return Err(CallControllerError::StaleCall);
            }
            running.video_commands.clone()
        };
        let (reply, result) = oneshot::channel();
        sender
            .send(command(reply))
            .await
            .map_err(|_| CallControllerError::WorkerStopped)?;
        result
            .await
            .map_err(|_| CallControllerError::WorkerStopped)??;
        Ok(())
    }

    async fn with_current_audio_controls(
        &self,
        token: u64,
        change: impl FnOnce(&AudioControls) -> Result<(), MediaError>,
    ) -> Result<(), CallControllerError> {
        if token == 0 || self.audio_token() != token {
            return Err(CallControllerError::StaleCall);
        }
        let account = self
            .manager
            .active_account()
            .await
            .ok_or(CallControllerError::AccountChanged)?;
        let slot = self.shared.slot.lock().await;
        let CallSlot::Running(running) = &*slot else {
            return Err(CallControllerError::StaleCall);
        };
        if running.presentation != token || running.cancel.is_none() {
            return Err(CallControllerError::StaleCall);
        }
        if running.account_id != account.id {
            return Err(CallControllerError::AccountChanged);
        }
        let controls = running
            .audio_controls
            .as_ref()
            .ok_or(CallControllerError::Media(MediaError::ControlNotRunning))?;
        change(controls)?;
        Ok(())
    }

    /// The exact call `status` describes, for a control to act on later (see
    /// `ObservedCall`): one snapshot of the status, the running call's own
    /// generation and its current join, read together under the slot's lock.
    /// A running status (starting, joined, reconnecting) is published only
    /// under that lock by the call in the slot, so `None` unless `status` is
    /// still current and names that call. An opening slot, a leave and a
    /// failed cleanup expose no exact generation.
    pub async fn observe(&self, status: &CallStatus) -> Option<ObservedCall> {
        let slot = self.shared.slot.lock().await;
        self.observe_slot(&slot, status)
    }

    /// `observe` without waiting, for a check on the UI thread: `Err` while
    /// the slot is busy, so the caller cannot tell.
    pub fn try_observe(
        &self,
        status: &CallStatus,
    ) -> Result<Option<ObservedCall>, tokio::sync::TryLockError> {
        let slot = self.shared.slot.try_lock()?;
        Ok(self.observe_slot(&slot, status))
    }

    fn observe_slot(&self, slot: &CallSlot, status: &CallStatus) -> Option<ObservedCall> {
        let (account_id, room_id) = match status {
            CallStatus::Starting {
                account_id,
                room_id,
            } => (account_id, room_id),
            CallStatus::Joined { account_id, call }
            | CallStatus::Reconnecting {
                account_id, call, ..
            } => (account_id, &call.room_id),
            _ => return None,
        };
        let CallSlot::Running(running) = slot else {
            return None;
        };
        if self.status() != *status
            || running.cancel.is_none()
            || running.account_id != *account_id
            || running.room_id != *room_id
        {
            return None;
        }
        Some(ObservedCall {
            account_id: account_id.clone(),
            room_id: room_id.clone(),
            call: running.token,
            presentation: running.presentation,
        })
    }

    /// Mute or unmute exactly the observed join (see `ObservedCall`): checked,
    /// with the caller's own `admit` (its security and account context from
    /// the click), under the slot's lock right before the change. Any other
    /// call or join is `StaleCall` and is not touched.
    pub async fn set_microphone_muted_for(
        &self,
        observed: &ObservedCall,
        muted: bool,
        admit: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<(), CallControllerError> {
        self.with_observed_audio_controls(observed, admit, |controls| {
            controls.set_microphone_muted(muted)
        })
        .await
    }

    /// Deafen or undeafen exactly the observed join, as
    /// `set_microphone_muted_for`.
    pub async fn set_deafened_for(
        &self,
        observed: &ObservedCall,
        deafened: bool,
        admit: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<(), CallControllerError> {
        self.with_observed_audio_controls(observed, admit, |controls| {
            controls.set_deafened(deafened)
        })
        .await
    }

    /// Stop a capture of exactly the observed join; it never starts one.
    /// Admission (as `set_microphone_muted_for`) and the enqueue on that
    /// call's own capture worker happen in one section under the slot's lock,
    /// which is this Stop's linearization point. The enqueue never waits: a
    /// full queue is `AlreadyBusy` and nothing is sent. Once queued, the Stop
    /// is not checked again when the worker applies it. The queue belongs to
    /// this call alone, so it can only stop this call's capture of `kind`
    /// (if that still runs), also after a rejoin of the same call, whose
    /// capture continues paused; it can never reach another call.
    pub async fn stop_video_capture_for(
        &self,
        observed: &ObservedCall,
        kind: CaptureKind,
        admit: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<(), CallControllerError> {
        let (reply, result) = oneshot::channel();
        {
            let slot = self.shared.slot.lock().await;
            let CallSlot::Running(running) = &*slot else {
                return Err(CallControllerError::StaleCall);
            };
            if !running.is_join(observed)
                || self.video_token() != observed.presentation
                || !self.status().controls_available()
                || !admit()
            {
                return Err(CallControllerError::StaleCall);
            }
            running
                .video_commands
                .try_send(CaptureCommand::Stop { kind, reply })
                .map_err(|error| match error {
                    mpsc::error::TrySendError::Full(_) => CallControllerError::AlreadyBusy,
                    mpsc::error::TrySendError::Closed(_) => CallControllerError::WorkerStopped,
                })?;
        }
        result
            .await
            .map_err(|_| CallControllerError::WorkerStopped)??;
        Ok(())
    }

    async fn with_observed_audio_controls(
        &self,
        observed: &ObservedCall,
        admit: &(dyn Fn() -> bool + Send + Sync),
        change: impl FnOnce(&AudioControls) -> Result<(), MediaError>,
    ) -> Result<(), CallControllerError> {
        let slot = self.shared.slot.lock().await;
        let CallSlot::Running(running) = &*slot else {
            return Err(CallControllerError::StaleCall);
        };
        if !running.is_join(observed) || self.audio_token() != observed.presentation || !admit() {
            return Err(CallControllerError::StaleCall);
        }
        let controls = running
            .audio_controls
            .as_ref()
            .ok_or(CallControllerError::Media(MediaError::ControlNotRunning))?;
        change(controls)?;
        Ok(())
    }

    /// Snapshot/read use no watch lock on a native audio callback. A new call
    /// token permanently invalidates an earlier ring even after that call ends.
    pub(crate) fn reserve_ringing_request(&self) -> Option<RingRequestLease> {
        self.shared.ring_request.reserve()
    }
    pub(crate) fn ringing_generation(&self) -> (u64, u64) {
        (
            self.shared.next_token.load(Ordering::Acquire),
            self.shared.ring_request.epoch.load(Ordering::Acquire),
        )
    }
    pub(crate) fn ringing_allowed(&self, generation: (u64, u64)) -> bool {
        self.shared.update_activity.idle()
            && self.shared.ring_request.allowed(generation.1)
            && self.ringing_generation() == generation
    }

    /// Immutable lock-free callback authority. The existing request epoch
    /// retires playback synchronously before transition waits; the call/update
    /// reservation covers opening/joining/cleanup and never adopts a later call.
    pub(crate) fn attachment_audio_authority(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let shared = self.shared.clone();
        let generation = self.ringing_generation();
        Arc::new(move || {
            shared.update_activity.idle()
                && shared.next_token.load(Ordering::Acquire) == generation.0
                && shared.ring_request.allowed(generation.1)
        })
    }

    pub fn status(&self) -> CallStatus {
        self.shared.status.borrow().clone()
    }

    /// Safe for a settings or call-entry screen: this contacts the homeserver
    /// but does not open a media device, publish a membership, or write a slot.
    pub async fn probe_audio(&self) -> Result<AudioCallCapability, CallControllerError> {
        if local_support() != LocalRtcSupport::NativeFrames {
            return Ok(AudioCallCapability::NativeRtcNotBuilt);
        }
        if !local_device_support().audio {
            return Ok(AudioCallCapability::AudioBackendNotBuilt);
        }
        let Some(account) = self.manager.active_account().await else {
            return Ok(AudioCallCapability::SignInRequired);
        };
        let sync = self.manager.subscribe_sync_status().borrow().clone();
        if !matches!(
            sync,
            SyncStatus::Running {
                backend: SyncBackend::SlidingSticky
            }
        ) {
            return Ok(AudioCallCapability::StickySyncRequired(sync));
        }
        let client = self.manager.active_client().await?;
        self.ensure_account(&account.id).await?;
        let rtc = RtcService::new(client);
        let transport = timeout(DISCOVERY_TIMEOUT, rtc.discover_transport())
            .await
            .map_err(|_| CallControllerError::TimedOut("discovering MatrixRTC transport"))??;
        self.ensure_account(&account.id).await?;
        Ok(match transport {
            TransportSupport::LiveKit { service_url } => AudioCallCapability::TransportReady {
                livekit_service_url: service_url,
            },
            TransportSupport::NoLiveKitTransport => AudioCallCapability::NoLiveKitTransport,
        })
    }

    /// Explicitly create the standard MatrixRTC call slot in a loaded room.
    /// This requires the user's room-state power level. An existing slot is
    /// not closed by `end_call()`, because other members may still be using it.
    pub async fn open_slot(
        &self,
        expected_account_id: &str,
        room_id: &str,
    ) -> Result<(), CallControllerError> {
        // Reserve before any account, encryption, or network await. The
        // operation runs independently of its UI caller so dropping that
        // caller cannot release the switch block while a state write runs.
        let (token, done) = self
            .shared
            .reserve_open_slot(expected_account_id, room_id)
            .await?;
        let controller = Self {
            manager: Arc::clone(&self.manager),
            shared: Arc::clone(&self.shared),
        };
        let shared = Arc::clone(&self.shared);
        let account_id = expected_account_id.to_owned();
        let room_id = room_id.to_owned();
        let (result_tx, result_rx) = oneshot::channel();
        tokio::spawn(async move {
            let result = controller.open_slot_reserved(&account_id, &room_id).await;
            let cleanup = shared.finish_open_slot(token).await;
            done.send_replace(true);
            // Preserve an earlier network error; the slot still retains any
            // independent native cleanup failure and its installation lease.
            let _ = result_tx.send(result.and(cleanup));
        });
        result_rx
            .await
            .map_err(|_| CallControllerError::WorkerStopped)?
    }

    async fn open_slot_reserved(
        &self,
        expected_account_id: &str,
        room_id: &str,
    ) -> Result<(), CallControllerError> {
        self.ensure_account(expected_account_id).await?;
        let account_id = self.ready_account_id().await?;
        if account_id != expected_account_id {
            return Err(CallControllerError::AccountChanged);
        }
        let client = self.manager.active_client().await?;
        self.ensure_account(&account_id).await?;
        let rtc = RtcService::new(client);
        let room = rtc
            .client()
            .rooms()
            .into_iter()
            .find(|room| room.room_id().as_str() == room_id)
            .ok_or(CallControllerError::RoomNotLoaded)?;
        let encryption = timeout(DISCOVERY_TIMEOUT, room.latest_encryption_state())
            .await
            .map_err(|_| CallControllerError::TimedOut("reading room encryption state"))?
            .map_err(|error| CallControllerError::RoomState(error.to_string()))?;
        if encryption.is_unknown() {
            return Err(CallControllerError::UnknownEncryptionState);
        }
        self.ensure_account(&account_id).await?;
        let slot_id = CallOptions::default().slot_id;
        timeout(
            JOIN_TIMEOUT,
            rtc.open_slot(room_id, &slot_id, encryption.is_encrypted()),
        )
        .await
        .map_err(|_| CallControllerError::TimedOut("opening MatrixRTC call slot"))??;
        self.ensure_account(&account_id).await
    }

    /// Join in the chosen membership mode and start microphone capture plus
    /// playback of subscribed remote microphones.
    pub async fn start_audio(
        &self,
        selection: AccountSelection,
        room_id: &str,
        mode: CallJoinMode,
        audio_devices: AudioDeviceSelection,
        intent: CallStartIntent,
        voice_spaces: Arc<VoiceSpaces>,
    ) -> Result<CallInfo, CallControllerError> {
        self.start_audio_owned(
            selection,
            room_id,
            mode.options(),
            audio_devices,
            intent,
            voice_spaces,
            None,
        )
        .await
    }

    /// Answer consumes the admitted capability. Caller must hold its existing
    /// account-transition lock until this method returns and race acquisition
    /// of that lock with accepted.cancelled(). No selected-room/preferred-mode
    /// UI input is accepted here. Setup revalidates immediately before join.
    pub(crate) async fn start_accepted_audio(
        &self,
        accepted: AcceptedOffer,
        audio_devices: AudioDeviceSelection,
        voice_spaces: Arc<VoiceSpaces>,
        _transition: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<CallInfo, CallControllerError> {
        let target = accepted
            .target()
            .map_err(|_| CallControllerError::IncomingOfferUnavailable)?;
        let mut waiting = accepted.startup_wait_guard();
        let result = self
            .start_audio_owned(
                target.selection().clone(),
                target.room_id(),
                target.options().clone(),
                audio_devices,
                CallStartIntent::Quiet,
                voice_spaces,
                Some(accepted),
            )
            .await;
        if result.is_ok() {
            waiting.complete();
        }
        result
    }

    async fn start_audio_owned(
        &self,
        selection: AccountSelection,
        room_id: &str,
        options: CallOptions,
        audio_devices: AudioDeviceSelection,
        intent: CallStartIntent,
        voice_spaces: Arc<VoiceSpaces>,
        accepted: Option<AcceptedOffer>,
    ) -> Result<CallInfo, CallControllerError> {
        if !selection.is_current() {
            return Err(CallControllerError::AccountChanged);
        }
        let account = tokio::select! {
            biased;
            _ = selection.revoked() => return Err(CallControllerError::AccountChanged),
            _ = accepted_cancelled(accepted.as_ref()) => return Err(CallControllerError::IncomingOfferUnavailable),
            account = self.manager.active_account() => account.ok_or(CallControllerError::Unavailable(
                AudioCallCapability::SignInRequired))?,
        };
        if account.id != selection.account_id() || !selection.is_current() {
            return Err(CallControllerError::AccountChanged);
        }
        let token = self.shared.next_token.fetch_add(1, Ordering::Relaxed);
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let (video_tx, video_rx) = mpsc::channel(4);
        let capture_stop = Arc::new(CaptureStop::default());
        let (done_tx, done_rx) = watch::channel(None);
        let (ready_tx, ready_rx) = oneshot::channel();
        {
            let mut slot = tokio::select! {
                biased;
                _ = selection.revoked() => return Err(CallControllerError::AccountChanged),
                _ = accepted_cancelled(accepted.as_ref()) => return Err(CallControllerError::IncomingOfferUnavailable),
                slot = self.shared.slot.lock() => slot,
            };
            if !selection.is_current() {
                return Err(CallControllerError::AccountChanged);
            }
            if !matches!(*slot, CallSlot::Idle) {
                return Err(CallControllerError::AlreadyBusy);
            }
            self.shared.reserve_update_lease()?;
            *slot = CallSlot::Running(RunningCall {
                token,
                presentation: token,
                account_id: account.id.clone(),
                room_id: room_id.to_owned(),
                cancel: Some(cancel_tx),
                done: done_rx,
                audio_controls: None,
                video_commands: video_tx,
                capture_stop: Arc::clone(&capture_stop),
            });
            // The call's own identity first: a reply that sees this
            // generation must also see whose call it is.
            self.shared.active_call_token.store(token, Ordering::SeqCst);
            self.shared
                .active_video_token
                .store(token, Ordering::SeqCst);
            self.shared
                .active_audio_token
                .store(token, Ordering::SeqCst);
            self.shared.video.send_replace(VideoUpdate::Clear { token });
            self.shared.roster.send_replace(RosterUpdate {
                token,
                members: Vec::new(),
            });
            self.shared
                .audio
                .send_replace(AudioUpdate::Unavailable { token });
            self.shared.local_video.send_replace(LocalVideoStatus {
                token,
                ..LocalVideoStatus::default()
            });
            self.shared
                .invitation
                .send_replace(InvitationUpdate { token, text: "" });
            self.shared.status.send_replace(CallStatus::Starting {
                account_id: account.id.clone(),
                room_id: room_id.to_owned(),
            });
        }
        let worker = CallWorker {
            manager: Arc::clone(&self.manager),
            shared: Arc::clone(&self.shared),
            account_id: account.id.clone(),
            account,
            room_id: room_id.to_owned(),
            options,
            audio_devices,
            token,
            presentation: token,
            cancel: cancel_rx,
            done: done_tx,
            ready: Some(ready_tx),
            video_commands: Some(video_rx),
            capture_stop,
            selection,
            intent,
            voice_spaces,
            accepted,
            established: None,
        };
        tokio::spawn(worker.run());
        ready_rx
            .await
            .map_err(|_| CallControllerError::WorkerStopped)?
    }

    /// Cancel capture and wait until the MatrixRTC leave has finished.
    /// A failed leave remains blocked. Only `abandon_failed_leave` can clear
    /// the block after the worker has terminated and local media has stopped.
    pub async fn end_call(&self) -> Result<(), CallControllerError> {
        self.end_call_matching(None, &|| true).await
    }

    /// Leave exactly the observed call (any of its joins), as `end_call`. Its
    /// call generation, account and room, and the caller's own `admit` (its
    /// security and account context from the click), are checked under the
    /// slot's lock in the same step that takes the call's cancellation. Any
    /// other call, an opening slot, a leave already under way and a failed
    /// cleanup are `StaleCall` and are not touched.
    pub async fn end_call_for(
        &self,
        observed: &ObservedCall,
        admit: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<(), CallControllerError> {
        self.end_call_matching(Some(observed), admit).await
    }

    async fn end_call_matching(
        &self,
        observed: Option<&ObservedCall>,
        admit: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<(), CallControllerError> {
        enum PriorOperation {
            Opening {
                token: u64,
                done: watch::Receiver<bool>,
            },
            CleanupFailed {
                token: u64,
                done: watch::Receiver<Option<Result<(), String>>>,
            },
        }
        let prior = {
            let slot = self.shared.slot.lock().await;
            match &*slot {
                // An observed leave acts only on its running call, below.
                _ if observed.is_some() => None,
                CallSlot::OpeningSlot {
                    token,
                    account_id,
                    room_id,
                    done,
                } => {
                    self.shared.status.send_replace(CallStatus::Ending {
                        account_id: account_id.clone(),
                        room_id: room_id.clone(),
                    });
                    Some(PriorOperation::Opening {
                        token: *token,
                        done: done.clone(),
                    })
                }
                CallSlot::CleanupFailed { token, done } => Some(PriorOperation::CleanupFailed {
                    token: *token,
                    done: done.clone(),
                }),
                _ => None,
            }
        };
        match prior {
            Some(PriorOperation::Opening { token, mut done }) => {
                timeout(SHUTDOWN_TIMEOUT, async {
                    loop {
                        if *done.borrow_and_update() {
                            return Ok::<(), CallControllerError>(());
                        }
                        done.changed()
                            .await
                            .map_err(|_| CallControllerError::WorkerStopped)?;
                    }
                })
                .await
                .map_err(|_| CallControllerError::TimedOut("waiting for call slot write"))??;
                let slot = self.shared.slot.lock().await;
                if let CallSlot::CleanupFailed {
                    token: current,
                    done,
                } = &*slot
                    && *current == token
                {
                    return match done.borrow().clone() {
                        Some(Err(reason)) => Err(CallControllerError::Cleanup(reason)),
                        _ => Err(CallControllerError::WorkerStopped),
                    };
                }
                return Ok(());
            }
            Some(PriorOperation::CleanupFailed { token, mut done }) => {
                let result = wait_cleanup_done(&mut done).await?;
                if let Err(reason) = result {
                    return Err(CallControllerError::Cleanup(reason));
                }
                let mut slot = self.shared.slot.lock().await;
                if matches!(&*slot, CallSlot::CleanupFailed { token: current, .. } if *current == token)
                {
                    *slot = CallSlot::Idle;
                    self.shared.release_update_lease();
                    self.shared.status.send_replace(CallStatus::Idle);
                }
                return Ok(());
            }
            None => {}
        }
        let (cancel, mut done, token, presentation, account_id, room_id) = {
            let mut slot = self.shared.slot.lock().await;
            if let Some(observed) = observed
                && !matches!(
                    &*slot,
                    CallSlot::Running(running) if running.is_call(observed) && admit()
                )
            {
                return Err(CallControllerError::StaleCall);
            }
            let running = match &mut *slot {
                CallSlot::Idle => return Ok(()),
                CallSlot::OpeningSlot { .. } | CallSlot::CleanupFailed { .. } => {
                    return Err(CallControllerError::AlreadyBusy);
                }
                CallSlot::Running(running) => running,
            };
            let cancel = running.cancel.take();
            running.capture_stop.request();
            if let Some(audio) = &running.audio_controls {
                audio.cancel();
            }
            let done = running.done.clone();
            self.shared.status.send_replace(CallStatus::Ending {
                account_id: running.account_id.clone(),
                room_id: running.room_id.clone(),
            });
            (
                cancel,
                done,
                running.token,
                running.presentation,
                running.account_id.clone(),
                running.room_id.clone(),
            )
        };
        if let Some(cancel) = cancel {
            let _ = cancel.send(());
        }
        self.shared.clear_media(token, presentation);
        let finished = timeout(SHUTDOWN_TIMEOUT, async {
            loop {
                if let Some(result) = done.borrow_and_update().clone() {
                    return result.map_err(CallControllerError::Cleanup);
                }
                done.changed()
                    .await
                    .map_err(|_| CallControllerError::WorkerStopped)?;
            }
        })
        .await;
        match finished {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error @ CallControllerError::WorkerStopped)) => {
                self.mark_cleanup_unknown(token, &account_id, &room_id, error.to_string())
                    .await;
                Err(error)
            }
            Ok(Err(error)) => Err(error),
            Err(_) => {
                let error = CallControllerError::TimedOut("waiting for call shutdown");
                self.mark_cleanup_unknown(token, &account_id, &room_id, error.to_string())
                    .await;
                Err(error)
            }
        }
    }

    /// Explicitly clear a failed leave after local media has stopped and the
    /// worker has ended. The caller must present a warning that the server may
    /// still show a membership; this does not retry or confirm server leave.
    pub async fn abandon_failed_leave(&self) -> Result<(), CallControllerError> {
        self.abandon_failed_leave_after_opener(morpheus_rtc::wait_for_audio_device_release())
            .await
    }

    // Keep the actual opener wait at the same lease-release boundary. The
    // private future parameter permits a held/failed receipt regression without
    // opening a device or mutating RTC's retained global owner in app tests.
    async fn abandon_failed_leave_after_opener(
        &self,
        opener: impl Future<Output = Result<(), morpheus_rtc::AudioDeviceReleaseError>>,
    ) -> Result<(), CallControllerError> {
        let (token, mut done) = {
            let slot = self.shared.slot.lock().await;
            match &*slot {
                CallSlot::Idle => return Ok(()),
                CallSlot::CleanupFailed { token, done } => (*token, done.clone()),
                _ => return Err(CallControllerError::AlreadyBusy),
            }
        };
        let _ = wait_cleanup_done(&mut done).await?;
        // A server-leave acknowledgement cannot abandon an unjoined native
        // opener or its late result. Keep the shared call/update lease held.
        opener
            .await
            .map_err(|error| CallControllerError::Cleanup(error.to_string()))?;
        morpheus_rtc::wait_for_ring_audio_release()
            .await
            .map_err(|error| CallControllerError::Cleanup(error.to_string()))?;
        // No other call can start while this slot is blocked, so any live
        // UI generation (including a rejoin's) belongs to this call.
        if self.shared.active_audio_token.load(Ordering::SeqCst) != 0
            || self.shared.active_video_token.load(Ordering::SeqCst) != 0
            || self.shared.active_capture_workers.load(Ordering::SeqCst) != 0
        {
            return Err(CallControllerError::Cleanup(
                "native capture is still stopping; try again when the device releases it".into(),
            ));
        }
        let mut slot = self.shared.slot.lock().await;
        if matches!(&*slot, CallSlot::CleanupFailed { token: current, .. } if *current == token) {
            *slot = CallSlot::Idle;
            self.shared.release_update_lease();
            self.shared.status.send_replace(CallStatus::Idle);
        }
        Ok(())
    }

    async fn mark_cleanup_unknown(
        &self,
        token: u64,
        account_id: &str,
        room_id: &str,
        reason: String,
    ) {
        let mut slot = self.shared.slot.lock().await;
        if let CallSlot::Running(running) = &*slot
            && running.token == token
        {
            let done = running.done.clone();
            *slot = CallSlot::CleanupFailed { token, done };
            self.shared.status.send_replace(CallStatus::CleanupFailed {
                account_id: account_id.to_owned(),
                room_id: room_id.to_owned(),
                reason,
            });
        }
    }

    async fn ready_account_id(&self) -> Result<String, CallControllerError> {
        match self.probe_audio().await? {
            AudioCallCapability::TransportReady { .. } => self
                .manager
                .active_account()
                .await
                .map(|account| account.id)
                .ok_or(CallControllerError::AccountChanged),
            unavailable => Err(CallControllerError::Unavailable(unavailable)),
        }
    }

    async fn ensure_account(&self, account_id: &str) -> Result<(), CallControllerError> {
        ensure_account(&self.manager, account_id).await
    }
}

async fn wait_cleanup_done(
    done: &mut watch::Receiver<Option<Result<(), String>>>,
) -> Result<Result<(), String>, CallControllerError> {
    timeout(SHUTDOWN_TIMEOUT, async {
        loop {
            if let Some(result) = done.borrow_and_update().clone() {
                return Ok(result);
            }
            done.changed()
                .await
                .map_err(|_| CallControllerError::WorkerStopped)?;
        }
    })
    .await
    .map_err(|_| CallControllerError::TimedOut("waiting for call worker"))?
}

// Read by the retained native audio opener as well as the async call owner. Account
// and sync invalidation must not wait for the owner's next select-loop poll.
// A transient sync loss is handled by the separate transmission gate at every
// push (and the worker's pause); only the end of sticky sync revokes capture.
fn audio_capture_allowed(stop: &CaptureStop, selection_current: bool, sync: &SyncStatus) -> bool {
    !stop.requested() && selection_current && signaling_health(sync) != SignalingHealth::Unavailable
}

fn sticky_sync_required(state: SyncStatus) -> CallControllerError {
    CallControllerError::Unavailable(AudioCallCapability::StickySyncRequired(state))
}

fn accepted_cancelled(
    accepted: Option<&AcceptedOffer>,
) -> impl Future<Output = ()> + Send + 'static + use<> {
    let guard = accepted.map(AcceptedOffer::pending_guard);
    async move {
        match guard {
            Some(guard) => guard.cancelled().await,
            None => pending().await,
        }
    }
}
async fn established_cancelled(guard: Option<&EstablishedGuard>) {
    match guard {
        Some(guard) => guard.cancelled().await,
        None => pending().await,
    }
}
fn incoming_capture_allowed(guard: Option<&EstablishedGuard>, info: &CallInfo) -> bool {
    guard.is_none_or(|guard| guard.allows(info))
}

/// Cancellation observes but never drops an already-started upstream join.
/// Caller still owns its timeout and explicitly leaves any returned call.
async fn settle_owned_join<J, C, F>(
    join: J,
    cancelled: C,
    on_cancel: F,
) -> (Option<J::Output>, bool)
where
    J: Future,
    C: Future<Output = ()>,
    F: FnOnce(),
{
    // Distinguish cancellation before the first join poll from cancellation
    // after any network work may have started. AtomicBool keeps this future
    // Send; all reads/writes are in this one async task.
    let started = AtomicBool::new(false);
    let join = async {
        started.store(true, Ordering::Relaxed);
        join.await
    };
    tokio::pin!(join);
    tokio::pin!(cancelled);
    tokio::select! {
        biased;
        () = &mut cancelled => {
            on_cancel();
            if started.load(Ordering::Relaxed) {
                (Some(join.await), true)
            } else {
                (None, true)
            }
        },
        result = &mut join => (Some(result), false),
    }
}

/// How one join's media session ended.
enum SessionEnd {
    /// Leave, cancellation, or an error that ends the call.
    Finish(Result<(), CallControllerError>),
    /// The call ended for everyone, e.g. its slot was closed.
    Ended(&'static str),
    /// The join is unusable while the user still wants the call.
    Rejoin(RejoinCause),
}

fn session_end(end: RtcCallEnd) -> SessionEnd {
    match end {
        RtcCallEnd::SlotClosed => SessionEnd::Ended("the room call was closed"),
        // No local Leave was requested, so the membership ended without us,
        // e.g. its delayed leave fired while this device was offline.
        RtcCallEnd::Left => SessionEnd::Rejoin(RejoinCause::MembershipEnded),
        RtcCallEnd::ConnectionLost => SessionEnd::Rejoin(RejoinCause::TransportClosed),
        RtcCallEnd::Removed => SessionEnd::Ended("the call service removed this device"),
    }
}

/// Why a rejoin installed no new join.
enum RejoinEnd {
    /// Leave (`Ok`), an account change, or a terminal failure. The worker's
    /// teardown still leaves any join that remains.
    Stopped(Result<(), CallControllerError>),
    /// A leave failed, so a membership may still be published.
    CleanupFailed(String),
}

enum JoinAttemptError {
    Stopped(RejoinEnd),
    Failed(CallControllerError),
}

/// What a rejoin keeps serving while no join is installed: Stop for running
/// captures, and the account and sync watches that can cancel it.
struct RecoveryInputs<'a> {
    outgoing: &'a mut LocalVideoSession,
    active: &'a mut watch::Receiver<Option<AccountInfo>>,
    sync: &'a mut watch::Receiver<SyncStatus>,
}

/// How a finished call is reported to the UI.
enum CallOutcome {
    Idle,
    Failed(String),
    Ended(String),
}

fn rejoin_exhausted(
    cause: RejoinCause,
    attempts: u32,
    last_error: Option<CallControllerError>,
) -> CallControllerError {
    let reason = cause.message();
    CallControllerError::ConnectionLost(match (attempts, last_error) {
        (0, _) => format!("{reason}; it did not recover within the reconnect window"),
        (attempts, Some(error)) => {
            format!("{reason}; rejoining failed after {attempts} attempt(s): {error}")
        }
        (attempts, None) => format!("{reason}; rejoining failed after {attempts} attempt(s)"),
    })
}

async fn leave_after(
    rtc: &mut RtcService,
    room_id: &str,
    result: Result<(), CallControllerError>,
) -> RejoinEnd {
    match leave_with_timeout(rtc, room_id).await {
        Ok(()) => RejoinEnd::Stopped(result),
        Err(error) => RejoinEnd::CleanupFailed(error.to_string()),
    }
}

fn signaling_ready(sync: &watch::Receiver<SyncStatus>) -> bool {
    signaling_health(&sync.borrow()) == SignalingHealth::Ready
}

/// Watchers that stop OS capture as soon as the account, sync session or
/// incoming-call authority behind it ends, even while the worker is
/// suspended in a publish or a rejoin. Dropping the value stops them.
struct CaptureGuards(Vec<tokio::task::JoinHandle<()>>);

impl Drop for CaptureGuards {
    fn drop(&mut self) {
        for guard in &self.0 {
            guard.abort();
        }
    }
}

fn revoke_capture(stop: &CaptureStop, audio: Option<&AudioControls>) {
    stop.request();
    if let Some(audio) = audio {
        audio.cancel();
    }
}

struct CallWorker {
    manager: Arc<AccountManager>,
    shared: Arc<Shared>,
    account_id: String,
    account: AccountInfo,
    room_id: String,
    options: CallOptions,
    audio_devices: AudioDeviceSelection,
    token: u64,
    presentation: u64,
    cancel: oneshot::Receiver<()>,
    done: watch::Sender<Option<Result<(), String>>>,
    ready: Option<oneshot::Sender<Result<CallInfo, CallControllerError>>>,
    video_commands: Option<mpsc::Receiver<CaptureCommand>>,
    capture_stop: Arc<CaptureStop>,
    selection: AccountSelection,
    intent: CallStartIntent,
    voice_spaces: Arc<VoiceSpaces>,
    accepted: Option<AcceptedOffer>,
    established: Option<EstablishedAuthorization>,
}

impl CallWorker {
    async fn run(mut self) {
        // The existing call reservation has already revoked every prior
        // attachment lease. Do not open any native call media until its actual
        // download/decoder/output owner releases; failure follows normal setup
        // cleanup, with no SDK join or microphone open entered here.
        let released = tokio::select! {
            biased;
            _ = &mut self.cancel => Err(CallControllerError::Cancelled),
            _ = self.selection.revoked() => Err(CallControllerError::AccountChanged),
            _ = accepted_cancelled(self.accepted.as_ref()) => Err(CallControllerError::IncomingOfferUnavailable),
            released = async {
                crate::audio_attachments::wait_for_release().await.map_err(|error| match error {
                    crate::audio_attachments::ReleaseError::TimedOut => CallControllerError::TimedOut("releasing attachment audio"),
                    error => CallControllerError::Cleanup(error.to_string()),
                })?;
                // The first call reservation permanently revoked the old ring.
                // Require its real native worker/TLS receipt before any SDK join
                // or call device open; unknown ownership keeps this call blocked.
                morpheus_rtc::wait_for_ring_audio_release().await
                    .map_err(|error| CallControllerError::Cleanup(error.to_string()))
            } => released,
        };
        let setup = match released {
            Ok(()) => self.setup().await,
            Err(error) => Err(error),
        };
        let (mut rtc, info) = match setup {
            Ok(joined) => joined,
            Err(error) => {
                let reason = error.to_string();
                let cancelled = matches!(error, CallControllerError::Cancelled);
                let cleanup = if matches!(error, CallControllerError::Cleanup(_)) {
                    Err(reason.clone())
                } else {
                    Ok(())
                };
                self.send_ready(Err(error));
                self.finish(cleanup, (!cancelled).then_some(reason)).await;
                return;
            }
        };

        if !matches!(
            self.cancel.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ) || !self.selection.is_current()
            || ensure_account(&self.manager, &self.account_id)
                .await
                .is_err()
            || !self.selection.is_current()
        {
            self.shared.clear_media(self.token, self.presentation);
            let leave = leave_with_timeout(&mut rtc, &self.room_id).await;
            self.send_ready(Err(CallControllerError::Cancelled));
            self.finish(leave.map_err(|error| error.to_string()), None)
                .await;
            return;
        }
        let incoming_guard = self
            .established
            .as_ref()
            .map(EstablishedAuthorization::guard);
        let capture_authorized = match incoming_guard.clone() {
            Some(guard) => {
                let expected = info.clone();
                self.capture_stop
                    .bind_authorization(move || guard.allows(&expected))
            }
            None => true,
        };
        if !capture_authorized || !incoming_capture_allowed(incoming_guard.as_ref(), &info) {
            let leave = leave_with_timeout(&mut rtc, &self.room_id).await;
            self.send_ready(Err(CallControllerError::IncomingOfferUnavailable));
            self.finish(leave.map_err(|error| error.to_string()), None)
                .await;
            return;
        }
        let mut active = self.manager.subscribe_active_account();
        let mut sync = self.manager.subscribe_sync_status();
        // Video pushes read live sync state themselves, so a worker suspended
        // in a publish cannot let frames through while signalling is down.
        let gate_sync = sync.clone();
        self.capture_stop
            .bind_transmission_gate(move || transmission_allowed(&gate_sync.borrow()));
        let created = self.new_audio_controls(&rtc, &info, incoming_guard.as_ref(), &sync);
        let controls = match created {
            Ok(controls) => controls,
            Err(error) => {
                let leave = leave_with_timeout(&mut rtc, &self.room_id).await;
                self.send_ready(Err(error.into()));
                self.finish(leave.map_err(|error| error.to_string()), None)
                    .await;
                return;
            }
        };
        {
            let mut slot = self.shared.slot.lock().await;
            match &mut *slot {
                CallSlot::Running(running)
                    if running.token == self.token
                        && running.cancel.is_some()
                        && self.selection.is_current()
                        && !self.capture_stop.requested()
                        && incoming_capture_allowed(incoming_guard.as_ref(), &info) =>
                {
                    running.audio_controls = Some(controls.clone());
                    self.shared.status.send_replace(CallStatus::Joined {
                        account_id: self.account_id.clone(),
                        call: info.clone(),
                    });
                }
                _ => {
                    drop(slot);
                    let leave = leave_with_timeout(&mut rtc, &self.room_id).await;
                    self.send_ready(Err(CallControllerError::Cancelled));
                    self.finish(leave.map_err(|error| error.to_string()), None)
                        .await;
                    return;
                }
            }
        }
        if self.send_ready(Ok(info.clone())) {
            let leave = leave_with_timeout(&mut rtc, &self.room_id).await;
            self.finish(leave.map_err(|error| error.to_string()), None)
                .await;
            return;
        }

        let account_still_active = active.borrow().as_ref().map(|account| account.id.as_str())
            == Some(self.account_id.as_str());
        let sync_state = sync.borrow().clone();
        // A transient sync loss is handled inside the session below.
        if !account_still_active
            || !incoming_capture_allowed(incoming_guard.as_ref(), &info)
            || !self.selection.is_current()
            || signaling_health(&sync_state) == SignalingHealth::Unavailable
        {
            self.shared.clear_media(self.token, self.presentation);
            let reason = if account_still_active && self.selection.is_current() {
                CallControllerError::Unavailable(AudioCallCapability::StickySyncRequired(
                    sync_state,
                ))
            } else {
                CallControllerError::AccountChanged
            };
            let leave = leave_with_timeout(&mut rtc, &self.room_id).await;
            let cleanup = leave.map_err(|error| error.to_string());
            self.finish(cleanup, Some(reason.to_string())).await;
            return;
        }
        let mut outgoing = LocalVideoSession::new(
            self.presentation,
            self.video_commands
                .take()
                .expect("call owns video commands"),
            self.shared.local_video.clone(),
            Arc::clone(&self.capture_stop),
            Arc::clone(&self.shared.active_capture_workers),
        );
        let local_voice_space = {
            let voice_spaces = Arc::clone(&self.voice_spaces);
            let voice_account = self.account_id.clone();
            let voice_room = self.room_id.clone();
            move || {
                voice_spaces
                    .local_rooms(&voice_account)
                    .contains(&voice_room)
            }
        };
        let mut info = info;
        let mut controls = controls;
        // Route choices made for this call (`set_audio_route`). One may be
        // queued already (Joined was published above), and one made while the
        // worker rejoins waits too: each join's start takes it
        // (`begin_session_route`).
        let mut route_requests = self.shared.route_requests.subscribe();
        // A rejoin resumes local audio where the lost join left it (devices
        // in use, a lost microphone still waited for, a speaker still
        // standing in) and then switches to the chosen route, rather than
        // opening that afresh.
        let mut resume_audio: Option<AudioResume> = None;
        let mut invitation_pending = self.intent == CallStartIntent::Notify;
        let mut invitation_deadline = None;
        let mut invitation_receipt = None;
        let mut recoveries = ReconnectWindow::per_call();
        if invitation_pending {
            self.shared.send_invitation(
                self.presentation,
                &self.selection,
                "Preparing invitation…",
            );
        }
        // One iteration per join. A lost join is left and replaced in place:
        // the call generation, its running captures and its Leave stay valid.
        let (outcome, rejoin_cleanup) = loop {
            // A platform publish operation can suspend the call worker. These
            // watchers stop OS capture immediately even if the worker has not
            // yet returned to its main account/sync select loop.
            let mut guards =
                self.spawn_capture_guards(&active, &sync, incoming_guard.as_ref(), Some(&controls));
            // A route chosen before this join started (right after Joined, or
            // while the worker was rejoining) applies from its first device
            // open, and becomes the baseline later requests are compared with.
            let start_retry = begin_session_route(
                &self.shared,
                &mut route_requests,
                self.presentation,
                &mut self.audio_devices,
            )
            .await;
            // Kept until this join's audio reports its own resume point: a join
            // lost before it opened anything resumes from the same point again.
            let mut resumed_from = resume_audio.take();
            let session = {
                let mut call_events = match rtc.take_call_events(&self.room_id) {
                    Ok(events) => events,
                    Err(error) => {
                        let error = CallControllerError::from(error);
                        break (CallOutcome::Failed(error.to_string()), Ok(()));
                    }
                };
                // Only the first join may notify; a rejoin is always quiet.
                let invitation = OutgoingRing::new(
                    &self.account,
                    self.selection.clone(),
                    &info,
                    self.options.interop_mode,
                    self.presentation,
                    &self.shared.active_video_token,
                    &self.capture_stop,
                    &local_voice_space,
                )
                .send(&rtc);
                tokio::pin!(invitation);
                // Owns its controls, so audio can start again inside this join
                // on new controls (see the route request branch below).
                let mut audio = run_local_audio(
                    &rtc,
                    &self.room_id,
                    controls.clone(),
                    resumed_from.clone(),
                    self.audio_devices.clone(),
                    start_retry,
                );
                let mut applied_audio = controls.subscribe_applied_state();
                let mut audio_route = controls.subscribe_route();
                let mut audio_failed = false;
                let video = run_remote_video(
                    &rtc,
                    &self.room_id,
                    self.presentation,
                    self.shared.video.clone(),
                    self.shared.roster.clone(),
                    self.shared.stage.subscribe(),
                    &self.shared.active_video_token,
                );
                tokio::pin!(video);
                let mut audio_running = true;
                let mut video_running = true;
                let mut media_degraded = false;
                let mut signaling_deadline = None;
                if signaling_health(&sync.borrow_and_update()) == SignalingHealth::Interrupted {
                    let _ = controls.set_transmission_suspended(true);
                    outgoing.pause_transmission();
                    signaling_deadline = Some(tokio::time::Instant::now() + SIGNALING_GRACE);
                } else {
                    outgoing.resume_transmission();
                }
                let paused = signaling_deadline.is_some();
                let status = self.session_status(&info, media_degraded, paused);
                self.shared
                    .publish_live_status(self.token, &self.capture_stop, status)
                    .await;
                loop {
                    tokio::select! {
                        biased;
                        _ = &mut self.cancel => break SessionEnd::Finish(Ok(())),
                        _ = established_cancelled(incoming_guard.as_ref()) => break SessionEnd::Finish(Err(CallControllerError::IncomingOfferUnavailable)),
                        _ = self.selection.revoked() => break SessionEnd::Finish(Err(CallControllerError::AccountChanged)),
                        changed = active.changed() => {
                            if changed.is_err() || !self.selection.is_current()
                                || active.borrow().as_ref().map(|account| account.id.as_str()) != Some(self.account_id.as_str()) {
                                break SessionEnd::Finish(Err(CallControllerError::AccountChanged));
                            }
                        }
                        changed = sync.changed() => {
                            let state = sync.borrow_and_update().clone();
                            let health = signaling_health(&state);
                            if changed.is_err() || health == SignalingHealth::Unavailable {
                                break SessionEnd::Finish(Err(CallControllerError::Unavailable(AudioCallCapability::StickySyncRequired(state))));
                            }
                            // Pause on loss, resume on return; the user's mute
                            // and capture choices are never rewritten here.
                            if health == SignalingHealth::Interrupted && signaling_deadline.is_none() {
                                let _ = controls.set_transmission_suspended(true);
                                outgoing.pause_transmission();
                                signaling_deadline = Some(tokio::time::Instant::now() + SIGNALING_GRACE);
                            } else if health == SignalingHealth::Ready && signaling_deadline.take().is_some() {
                                let _ = controls.set_transmission_suspended(false);
                                outgoing.resume_transmission();
                            }
                            let status = self.session_status(&info, media_degraded, signaling_deadline.is_some());
                            self.shared.publish_live_status(self.token, &self.capture_stop, status).await;
                        }
                        // Past the grace period the membership's delayed leave
                        // may have fired, so this join can no longer be trusted.
                        _ = tokio::time::sleep_until(signaling_deadline.unwrap_or_else(tokio::time::Instant::now)), if signaling_deadline.is_some() => {
                            break SessionEnd::Rejoin(RejoinCause::SignalingTimeout);
                        }
                        event = call_events.recv() => match event {
                            Ok(event) => {
                                #[cfg(all(test, target_os = "linux"))]
                                if let morpheus_rtc::RtcCallEvent::KeyImported { member_id, identity, key_index } = &event {
                                    let bounded = member_id.len() <= 512 && identity.len() <= 512;
                                    let _ = self.shared.recovery_keys.send(RecoveryKeyObservation {
                                        bounded,
                                        presentation: self.presentation,
                                        member_id: if bounded { member_id.clone() } else { String::new() },
                                        identity: if bounded { identity.clone() } else { String::new() },
                                        key_index: *key_index,
                                    });
                                }
                                match RtcConnectionSignal::from_event(&event) {
                                Some(RtcConnectionSignal::Ended(end)) => break session_end(end),
                                Some(signal) => {
                                    let degraded = signal == RtcConnectionSignal::MediaDegraded;
                                    if degraded != media_degraded {
                                        media_degraded = degraded;
                                        let status = self.session_status(&info, media_degraded, signaling_deadline.is_some());
                                        self.shared.publish_live_status(self.token, &self.capture_stop, status).await;
                                    }
                                }
                                None => {}
                                }
                            },
                            // A missed health change is re-reported by the next
                            // signal; remote audio and video resynchronise their
                            // participants and tracks on their own refresh ticks.
                            Err(broadcast::error::RecvError::Lagged(_)) => {}
                            Err(broadcast::error::RecvError::Closed) => break SessionEnd::Rejoin(RejoinCause::TransportClosed),
                        },
                        outcome = &mut invitation, if invitation_pending => {
                            invitation_pending = false;
                            self.shared.send_invitation(self.presentation, &self.selection, outcome.status_text());
                            if let InvitationOutcome::Sent(receipt) = outcome {
                                invitation_deadline = Some(tokio::time::Instant::from_std(receipt.expires_at()));
                                invitation_receipt = Some(receipt);
                            }
                        }
                        _ = tokio::time::sleep_until(invitation_deadline.unwrap_or_else(tokio::time::Instant::now)), if invitation_deadline.is_some() => {
                            invitation_deadline = None;
                            self.shared.send_invitation(self.presentation, &self.selection,
                                "Invitation sent; its 30-second window has ended.");
                        }
                        result = &mut audio, if audio_running => match result {
                            // The audio loop returns only once the call ended;
                            // the call event branch above reports how.
                            Ok(()) => audio_running = false,
                            Err(MediaError::CallEnded) => break SessionEnd::Finish(Err(CallControllerError::Media(MediaError::CallEnded))),
                            Err(error) => {
                                // A missing or stuck local audio device must not
                                // tear down the receive-only call stage.
                                audio_running = false;
                                audio_failed = true;
                                self.shared.fail_audio(self.presentation, AudioFailure::from_error(&error));
                            }
                        },
                        changed = applied_audio.changed() => {
                            if changed.is_ok() && controls.is_running() {
                                self.shared.send_applied_audio(self.presentation, *applied_audio.borrow_and_update());
                            }
                        }
                        changed = audio_route.changed() => {
                            if changed.is_ok() {
                                let route = audio_route.borrow_and_update().clone();
                                self.shared.send_audio_route(self.presentation, route);
                            }
                        }
                        changed = route_requests.changed() => {
                            let request = route_requests.borrow_and_update().clone();
                            // Another generation's choice never reroutes this one.
                            if changed.is_ok() && request.token == self.presentation {
                                // The current choice: a rejoin opens it too.
                                self.audio_devices = request.selection.clone();
                                if audio_running {
                                    // The audio loop opens the new devices beside
                                    // the ones in use and reports the outcome.
                                    let _ = controls.request_route(request.selection, request.retry);
                                } else if audio_failed {
                                    // Restart where the stopped loop left off, so a
                                    // lost or unresumed microphone is not reopened by
                                    // its route on a request that did not choose one.
                                    let resume = controls.resume_point().or_else(|| resumed_from.clone());
                                    let fresh = self.restarted_audio_controls(
                                        &rtc,
                                        &info,
                                        incoming_guard.as_ref(),
                                        &sync,
                                        &controls,
                                        signaling_deadline.is_some(),
                                    );
                                    // Awaits with only the worker's Sync parts:
                                    // CallWorker is not Sync, and this future
                                    // must stay Send.
                                    if let Some(fresh) = fresh
                                        && let Some(restarted) = install_restarted_audio(
                                            &self.shared,
                                            self.token,
                                            self.presentation,
                                            &self.selection,
                                            &self.capture_stop,
                                            fresh,
                                        )
                                        .await
                                    {
                                        // New controls, carrying the requested mute and
                                        // deafen state, replace the stopped ones; the
                                        // capture guards revoke them from now on.
                                        controls = restarted;
                                        guards = self.spawn_capture_guards(&active, &sync, incoming_guard.as_ref(), Some(&controls));
                                        applied_audio = controls.subscribe_applied_state();
                                        audio_route = controls.subscribe_route();
                                        // Opening, not the route from before audio stopped.
                                        self.shared.send_audio_route(self.presentation, controls.route());
                                        audio = run_local_audio(&rtc, &self.room_id, controls.clone(), resume.clone(), self.audio_devices.clone(), request.retry);
                                        // Kept until the restarted loop reports its own.
                                        resumed_from = resume;
                                        audio_running = true;
                                        audio_failed = false;
                                    }
                                }
                            }
                        }
                        // A closed roster/video stream must not end microphone
                        // audio. run_remote_video normally waits for cancellation.
                        () = &mut video, if video_running => video_running = false,
                        event = outgoing.next_event() => {
                            // A capture command may be ready in the same select
                            // as Leave or an account change. Check the current
                            // generation before opening any OS capture device.
                            if self.cancel.try_recv().is_ok() {
                                break SessionEnd::Finish(Ok(()));
                            }
                            if self.shared.active_video_token.load(Ordering::SeqCst) != self.presentation
                                || !incoming_capture_allowed(incoming_guard.as_ref(), &info)
                                || !self.selection.is_current()
                                || active.borrow().as_ref().map(|account| account.id.as_str())
                                    != Some(self.account_id.as_str())
                            {
                                break SessionEnd::Finish(Err(CallControllerError::AccountChanged));
                            }
                            outgoing.handle_event(&rtc, &self.room_id, event).await;
                        },
                    }
                }
            };
            drop(guards);
            controls.cancel();
            let cause = match session {
                SessionEnd::Finish(Ok(())) => break (CallOutcome::Idle, Ok(())),
                SessionEnd::Finish(Err(error)) => {
                    break (CallOutcome::Failed(error.to_string()), Ok(()));
                }
                SessionEnd::Ended(reason) => break (CallOutcome::Ended(reason.to_owned()), Ok(())),
                SessionEnd::Rejoin(cause) => cause,
            };
            if incoming_guard.is_some() {
                // An answered call is authorized for its admitted join only;
                // a fresh membership would need a new Answer.
                let error = CallControllerError::ConnectionLost(format!(
                    "{}; an answered incoming call is not rejoined automatically",
                    cause.message()
                ));
                break (CallOutcome::Failed(error.to_string()), Ok(()));
            }
            if self.capture_stop.requested() {
                let error = CallControllerError::AccountChanged;
                break (CallOutcome::Failed(error.to_string()), Ok(()));
            }
            if !recoveries.try_start(std::time::Instant::now()) {
                let error = CallControllerError::ConnectionLost(format!(
                    "{}; it kept dropping after rejoining",
                    cause.message()
                ));
                break (CallOutcome::Failed(error.to_string()), Ok(()));
            }
            // The cancelled audio future may have owned a pending native open.
            // No new membership/media generation can treat its guard as joined.
            if let Err(error) = morpheus_rtc::wait_for_audio_device_release().await {
                let reason = error.to_string();
                break (CallOutcome::Failed(reason.clone()), Err(reason));
            }
            // A pending invitation named the lost membership; it is never resent.
            let invitation_interrupted = std::mem::take(&mut invitation_pending);
            outgoing.detach_publications();
            if !self.advance_presentation(&mut outgoing).await {
                // Leave or an account change won before the rejoin began.
                break match self.stop_result() {
                    Ok(()) => (CallOutcome::Idle, Ok(())),
                    Err(error) => (CallOutcome::Failed(error.to_string()), Ok(())),
                };
            }
            if invitation_interrupted {
                self.shared.send_invitation(
                    self.presentation,
                    &self.selection,
                    INVITATION_INTERRUPTED,
                );
            }
            let requested = controls.requested_state();
            resume_audio = Some(rejoin_resume_point(
                controls.resume_point(),
                resumed_from,
                &self.audio_devices,
            ));
            let guards = self.spawn_capture_guards(&active, &sync, None, None);
            let mut inputs = RecoveryInputs {
                outgoing: &mut outgoing,
                active: &mut active,
                sync: &mut sync,
            };
            let rejoined = self
                .rejoin(&mut rtc, cause, &info, requested, &mut inputs)
                .await;
            drop(guards);
            match rejoined {
                Ok((joined, rejoined_controls)) => {
                    info = joined;
                    controls = rejoined_controls;
                }
                Err(RejoinEnd::Stopped(Ok(()))) => break (CallOutcome::Idle, Ok(())),
                Err(RejoinEnd::Stopped(Err(error))) => {
                    break (CallOutcome::Failed(error.to_string()), Ok(()));
                }
                Err(RejoinEnd::CleanupFailed(reason)) => break (CallOutcome::Idle, Err(reason)),
            }
        };
        self.capture_stop.request();
        controls.cancel();
        let outgoing_result = outgoing.shutdown().await;
        self.shared.clear_media(self.token, self.presentation);
        // Local media and invitation UI stop before the bounded network
        // cleanup. Unknown HTTP completion has no receipt and is not retried;
        // the invitation's original short expiry remains the fallback.
        if let Some(receipt) = invitation_receipt {
            let _ = receipt.redact(&rtc).await;
        }
        let leave_result = match leave_with_timeout(&mut rtc, &self.room_id).await {
            // A rejoin that failed after leaving the lost join left nothing.
            Err(CallControllerError::Rtc(RtcError::NotJoined)) => Ok(()),
            result => result,
        };
        let mut problems = Vec::new();
        if let Err(reason) = rejoin_cleanup {
            problems.push(reason);
        }
        if let Err(error) = leave_result {
            problems.push(error.to_string());
        }
        if let Err(video) = outgoing_result {
            problems.push(format!("video cleanup: {video}"));
        }
        let cleanup = if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        };
        self.finish_with(cleanup, outcome).await;
    }

    async fn setup(&mut self) -> Result<(RtcService, CallInfo), CallControllerError> {
        if !self.selection.is_current() {
            return Err(CallControllerError::AccountChanged);
        }
        let controller = CallController {
            manager: Arc::clone(&self.manager),
            shared: Arc::clone(&self.shared),
        };
        let probe = tokio::select! {
            biased;
            _ = &mut self.cancel => return Err(CallControllerError::Cancelled),
            _ = self.selection.revoked() => return Err(CallControllerError::AccountChanged),
            _ = accepted_cancelled(self.accepted.as_ref()) => return Err(CallControllerError::IncomingOfferUnavailable),
            result = controller.probe_audio() => result?,
        };
        match probe {
            AudioCallCapability::TransportReady { .. } => {}
            unavailable => return Err(CallControllerError::Unavailable(unavailable)),
        }
        tokio::select! {
            biased;
            _ = &mut self.cancel => return Err(CallControllerError::Cancelled),
            _ = self.selection.revoked() => return Err(CallControllerError::AccountChanged),
            _ = accepted_cancelled(self.accepted.as_ref()) => return Err(CallControllerError::IncomingOfferUnavailable),
            result = ensure_account(&self.manager, &self.account_id) => result?,
        }
        if !self.selection.is_current() {
            return Err(CallControllerError::AccountChanged);
        }
        let client = tokio::select! {
            biased;
            _ = &mut self.cancel => return Err(CallControllerError::Cancelled),
            _ = self.selection.revoked() => return Err(CallControllerError::AccountChanged),
            _ = accepted_cancelled(self.accepted.as_ref()) => return Err(CallControllerError::IncomingOfferUnavailable),
            result = self.manager.active_client() => result?,
        };
        tokio::select! {
            biased;
            _ = &mut self.cancel => return Err(CallControllerError::Cancelled),
            _ = self.selection.revoked() => return Err(CallControllerError::AccountChanged),
            _ = accepted_cancelled(self.accepted.as_ref()) => return Err(CallControllerError::IncomingOfferUnavailable),
            result = ensure_account(&self.manager, &self.account_id) => result?,
        }
        if !self.selection.is_current() {
            return Err(CallControllerError::AccountChanged);
        }
        let mut rtc = RtcService::new(client);
        if !matches!(
            self.cancel.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ) {
            return Err(CallControllerError::Cancelled);
        }
        if let Some(accepted) = &mut self.accepted {
            tokio::select! {
                biased;
                _ = &mut self.cancel => return Err(CallControllerError::Cancelled),
                result = accepted.revalidate_before_join(&self.manager, &self.voice_spaces) =>
                    result.map_err(|_| CallControllerError::IncomingOfferUnavailable)?,
            }
            let target = accepted
                .begin_join()
                .map_err(|_| CallControllerError::IncomingOfferUnavailable)?;
            // Only this admitted target/options is used; no room selection,
            // preferred protocol, slot creation, notification, or fallback.
            self.room_id = target.room_id().to_owned();
            self.options = target.options().clone();
        }
        let room_id = self.room_id.clone();
        // Keep joining alive after cancellation: a membership can already have
        // been published. The same existing bound applies, then cleanup leaves
        // any returned handle before releasing the call slot/update lease.
        let joined = {
            let join = timeout(JOIN_TIMEOUT, rtc.join(&room_id, self.options.clone()));
            let cancel = &mut self.cancel;
            let selection = &self.selection;
            let offer_cancelled = accepted_cancelled(self.accepted.as_ref());
            let ready = &mut self.ready;
            let cancelled = async {
                tokio::select! {
                    _ = cancel => {},
                    _ = selection.revoked() => {},
                    _ = offer_cancelled => {},
                }
            };
            settle_owned_join(join, cancelled, || {
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Err(CallControllerError::Cancelled));
                }
            })
            .await
        };
        let (joined, cancelled) = joined;
        let Some(joined) = joined else {
            return Err(CallControllerError::Cancelled);
        };
        let info = match joined {
            Ok(Ok(info)) => info,
            Ok(Err(error)) => {
                return Err(cleanup_after_failed_join(&mut rtc, &room_id, error.into()).await);
            }
            Err(_) => {
                let original = CallControllerError::TimedOut("joining MatrixRTC call");
                return Err(cleanup_after_failed_join(&mut rtc, &room_id, original).await);
            }
        };
        if cancelled {
            leave_with_timeout(&mut rtc, &room_id)
                .await
                .map_err(|error| CallControllerError::Cleanup(error.to_string()))?;
            return Err(CallControllerError::Cancelled);
        }
        let account_check = tokio::select! {
            biased;
            _ = &mut self.cancel => Err(CallControllerError::Cancelled),
            _ = self.selection.revoked() => Err(CallControllerError::AccountChanged),
            _ = accepted_cancelled(self.accepted.as_ref()) => Err(CallControllerError::IncomingOfferUnavailable),
            result = ensure_account(&self.manager, &self.account_id) => result,
        };
        let account_check = account_check.and_then(|()| {
            self.selection
                .is_current()
                .then_some(())
                .ok_or(CallControllerError::AccountChanged)
        });
        if let Err(error) = account_check {
            leave_with_timeout(&mut rtc, &room_id)
                .await
                .map_err(|cleanup| {
                    CallControllerError::Cleanup(format!(
                        "{error}; cleanup after account change also failed: {cleanup}"
                    ))
                })?;
            return Err(error);
        }
        if let Some(accepted) = self.accepted.take() {
            let authorized = tokio::select! {
                biased;
                _ = &mut self.cancel => Err(CallControllerError::Cancelled),
                _ = self.selection.revoked() => Err(CallControllerError::AccountChanged),
                result = accepted.commit_join(&self.manager, &self.voice_spaces, &rtc, &info) =>
                    result.map_err(|_| CallControllerError::IncomingOfferUnavailable),
            };
            match authorized {
                Ok(owner) => self.established = Some(owner),
                Err(error) => {
                    leave_with_timeout(&mut rtc, &room_id)
                        .await
                        .map_err(|cleanup| CallControllerError::Cleanup(cleanup.to_string()))?;
                    return Err(error);
                }
            }
        }
        Ok((rtc, info))
    }

    fn new_audio_controls(
        &self,
        rtc: &RtcService,
        info: &CallInfo,
        incoming_guard: Option<&EstablishedGuard>,
        sync: &watch::Receiver<SyncStatus>,
    ) -> Result<AudioControls, MediaError> {
        let selection = self.selection.clone();
        let stop = Arc::clone(&self.capture_stop);
        let allowed_sync = sync.clone();
        let transmit_sync = sync.clone();
        let incoming = incoming_guard.cloned();
        let info = info.clone();
        // The second predicate is read at every microphone push, independent
        // of this worker: a transient sync loss withholds audio, not revokes it.
        rtc.audio_controls_gated(
            &self.room_id,
            move || {
                audio_capture_allowed(&stop, selection.is_current(), &allowed_sync.borrow())
                    && incoming_capture_allowed(incoming.as_ref(), &info)
            },
            move || transmission_allowed(&transmit_sync.borrow()),
        )
    }

    /// Controls for starting local audio again inside the same join after its
    /// loop stopped (a chosen device missing at the start, or another open
    /// still in progress), for the route the user just chose. They carry the
    /// stopped ones' requested mute and deafen state, so nothing is unmuted
    /// that was not, and a signalling pause still holds. Install them with
    /// `install_restarted_audio`.
    fn restarted_audio_controls(
        &self,
        rtc: &RtcService,
        info: &CallInfo,
        incoming_guard: Option<&EstablishedGuard>,
        sync: &watch::Receiver<SyncStatus>,
        stopped: &AudioControls,
        suspended: bool,
    ) -> Option<AudioControls> {
        let controls = self
            .new_audio_controls(rtc, info, incoming_guard, sync)
            .ok()?;
        if controls
            .preset_requested_state(stopped.requested_state())
            .is_err()
            || (suspended && controls.set_transmission_suspended(true).is_err())
        {
            controls.cancel();
            return None;
        }
        Some(controls)
    }

    fn spawn_capture_guards(
        &self,
        active: &watch::Receiver<Option<AccountInfo>>,
        sync: &watch::Receiver<SyncStatus>,
        incoming_guard: Option<&EstablishedGuard>,
        audio: Option<&AudioControls>,
    ) -> CaptureGuards {
        let mut guards = Vec::with_capacity(3);
        guards.push({
            let mut active = active.clone();
            let account_id = self.account_id.clone();
            let selection = self.selection.clone();
            let stop = Arc::clone(&self.capture_stop);
            let audio = audio.cloned();
            tokio::spawn(async move {
                loop {
                    if !selection.is_current()
                        || active.borrow().as_ref().map(|account| account.id.as_str())
                            != Some(account_id.as_str())
                    {
                        revoke_capture(&stop, audio.as_ref());
                        break;
                    }
                    tokio::select! {
                        _ = selection.revoked() => {
                            revoke_capture(&stop, audio.as_ref());
                            break;
                        }
                        changed = active.changed() => {
                            if changed.is_err() {
                                revoke_capture(&stop, audio.as_ref());
                                break;
                            }
                        }
                    }
                }
            })
        });
        guards.push({
            let mut sync = sync.clone();
            let stop = Arc::clone(&self.capture_stop);
            let audio = audio.cloned();
            tokio::spawn(async move {
                loop {
                    // A transient sync loss pauses media in the worker; only
                    // the end of sticky sync for this session revokes capture.
                    if signaling_health(&sync.borrow()) == SignalingHealth::Unavailable {
                        revoke_capture(&stop, audio.as_ref());
                        break;
                    }
                    if sync.changed().await.is_err() {
                        revoke_capture(&stop, audio.as_ref());
                        break;
                    }
                }
            })
        });
        if let Some(guard) = incoming_guard.cloned() {
            let stop = Arc::clone(&self.capture_stop);
            let audio = audio.cloned();
            guards.push(tokio::spawn(async move {
                guard.cancelled().await;
                revoke_capture(&stop, audio.as_ref());
            }));
        }
        CaptureGuards(guards)
    }

    /// Joined, or reconnecting inside the same join. Signalling outranks
    /// media because it also pauses outgoing media.
    fn session_status(&self, info: &CallInfo, media_degraded: bool, paused: bool) -> CallStatus {
        if paused {
            self.reconnecting(info, CallInterruption::Signaling)
        } else if media_degraded {
            self.reconnecting(info, CallInterruption::Media)
        } else {
            CallStatus::Joined {
                account_id: self.account_id.clone(),
                call: info.clone(),
            }
        }
    }

    fn reconnecting(&self, info: &CallInfo, interruption: CallInterruption) -> CallStatus {
        CallStatus::Reconnecting {
            account_id: self.account_id.clone(),
            call: info.clone(),
            interruption,
        }
    }

    async fn publish_recovery(&mut self, status: CallStatus) {
        self.shared
            .publish_live_status(self.token, &self.capture_stop, status)
            .await;
    }

    /// Leave (`Ok`) or an account change, as observed while rejoining.
    fn stop_result(&self) -> Result<(), CallControllerError> {
        if self.selection.is_current() {
            Ok(())
        } else {
            Err(CallControllerError::AccountChanged)
        }
    }

    /// Start a new UI generation before rejoining. Every frame, roster and
    /// audio update queued for the ended join then fails the UI's token
    /// checks, while the call's capture authority and Leave stay untouched.
    async fn advance_presentation(&mut self, outgoing: &mut LocalVideoSession) -> bool {
        let generation = self.shared.next_token.fetch_add(1, Ordering::Relaxed);
        let previous = self.presentation;
        let mut slot = self.shared.slot.lock().await;
        let CallSlot::Running(running) = &mut *slot else {
            return false;
        };
        if running.token != self.token
            || running.presentation != previous
            || running.cancel.is_none()
            || self.capture_stop.requested()
        {
            return false;
        }
        let swapped = self
            .shared
            .active_video_token
            .compare_exchange(previous, generation, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if !swapped {
            return false;
        }
        // A failed local audio device cleared the old audio generation; the
        // rejoined membership opens audio again under the new one.
        self.shared
            .active_audio_token
            .store(generation, Ordering::SeqCst);
        running.presentation = generation;
        self.presentation = generation;
        // A pin/page is presentation intent, not join authority. Retag only
        // choices belonging to this call; never inherit another call's pin.
        self.shared.stage.send_modify(|request| {
            if request.token == previous {
                request.token = generation;
            }
        });
        // So is a device choice: one accepted just before the join was lost
        // is carried to the rejoin as its chosen route (never as a retry)
        // instead of being dropped as another call's. What the rejoin opens
        // still follows its own rules (`rejoin_resume_point`), so this opens
        // nothing by itself; the reply asks the user to check the devices and
        // choose again (`route_reply_notice`).
        self.shared.retag_route_for_rejoin(previous, generation);
        // No device is open until the rejoined loop reports its own route.
        self.shared
            .audio_route
            .send_replace(AudioRouteUpdate::default());
        self.shared
            .video
            .send_replace(VideoUpdate::Clear { token: generation });
        self.shared.roster.send_replace(RosterUpdate {
            token: generation,
            members: Vec::new(),
        });
        self.shared
            .audio
            .send_replace(AudioUpdate::Unavailable { token: generation });
        self.shared.invitation.send_replace(InvitationUpdate {
            token: generation,
            text: "",
        });
        outgoing.set_presentation_token(generation);
        true
    }

    /// Wait until `until`, or with `need_signaling` until sticky sync is
    /// ready again, honouring Leave, account changes and terminal sync loss.
    /// Returns false when `until` passed while still waiting for sync.
    async fn recovery_pause(
        &mut self,
        inputs: &mut RecoveryInputs<'_>,
        until: tokio::time::Instant,
        need_signaling: bool,
    ) -> Result<bool, RejoinEnd> {
        let sleep = tokio::time::sleep_until(until);
        tokio::pin!(sleep);
        loop {
            let state = inputs.sync.borrow_and_update().clone();
            match signaling_health(&state) {
                SignalingHealth::Ready if need_signaling => return Ok(true),
                SignalingHealth::Unavailable => {
                    return Err(RejoinEnd::Stopped(Err(sticky_sync_required(state))));
                }
                _ => {}
            }
            tokio::select! {
                biased;
                _ = &mut self.cancel => return Err(RejoinEnd::Stopped(Ok(()))),
                _ = self.selection.revoked() => {
                    return Err(RejoinEnd::Stopped(Err(CallControllerError::AccountChanged)));
                }
                changed = inputs.active.changed() => {
                    if changed.is_err()
                        || !self.selection.is_current()
                        || inputs.active.borrow().as_ref().map(|account| account.id.as_str())
                            != Some(self.account_id.as_str())
                    {
                        return Err(RejoinEnd::Stopped(Err(CallControllerError::AccountChanged)));
                    }
                }
                changed = inputs.sync.changed() => {
                    if changed.is_err() {
                        let state = inputs.sync.borrow().clone();
                        return Err(RejoinEnd::Stopped(Err(sticky_sync_required(state))));
                    }
                }
                event = inputs.outgoing.next_event() => {
                    inputs.outgoing.handle_detached_event(event).await;
                }
                () = &mut sleep => return Ok(!need_signaling),
            }
        }
    }

    /// One fresh join with the original options. Cancellation observes but
    /// never drops a join already in flight; a join that completes after
    /// Leave or an account change is left again before returning.
    async fn join_attempt(
        &mut self,
        rtc: &mut RtcService,
        outgoing: &mut LocalVideoSession,
        remaining: Duration,
    ) -> Result<CallInfo, JoinAttemptError> {
        let room_id = self.room_id.clone();
        let (joined, cancelled) = {
            let join = timeout(
                remaining.min(JOIN_TIMEOUT),
                rtc.join(&room_id, self.options.clone()),
            );
            let cancel = &mut self.cancel;
            let selection = &self.selection;
            let cancelled = async {
                loop {
                    tokio::select! {
                        biased;
                        _ = &mut *cancel => break,
                        _ = selection.revoked() => break,
                        event = outgoing.next_event() => {
                            outgoing.handle_detached_event(event).await;
                        }
                    }
                }
            };
            settle_owned_join(join, cancelled, || {}).await
        };
        let Some(joined) = joined else {
            let stopped = RejoinEnd::Stopped(self.stop_result());
            return Err(JoinAttemptError::Stopped(stopped));
        };
        let result = match joined {
            Ok(Ok(info)) => Ok(info),
            Ok(Err(error)) => {
                let original = CallControllerError::from(error);
                Err(cleanup_after_failed_join(rtc, &room_id, original).await)
            }
            Err(_) => {
                let original = CallControllerError::TimedOut("rejoining MatrixRTC call");
                Err(cleanup_after_failed_join(rtc, &room_id, original).await)
            }
        };
        if cancelled {
            let end = match result {
                Ok(_) => leave_after(rtc, &self.room_id, self.stop_result()).await,
                Err(CallControllerError::Cleanup(reason)) => RejoinEnd::CleanupFailed(reason),
                Err(_) => RejoinEnd::Stopped(self.stop_result()),
            };
            return Err(JoinAttemptError::Stopped(end));
        }
        let info = result.map_err(JoinAttemptError::Failed)?;
        let account = ensure_account(&self.manager, &self.account_id).await;
        if account.is_err() || !self.selection.is_current() {
            let changed = Err(CallControllerError::AccountChanged);
            let end = leave_after(rtc, &self.room_id, changed).await;
            return Err(JoinAttemptError::Stopped(end));
        }
        Ok(info)
    }

    /// Leave the unusable join once the homeserver is reachable, then make a
    /// bounded number of fresh joins with the original options: never a
    /// notification, another protocol, or an old token or endpoint. Each join
    /// repeats discovery, encryption readiness and the transport's HTTPS
    /// authorization and WSS media endpoint checks.
    async fn rejoin(
        &mut self,
        rtc: &mut RtcService,
        cause: RejoinCause,
        last: &CallInfo,
        requested: AudioState,
        inputs: &mut RecoveryInputs<'_>,
    ) -> Result<(CallInfo, AudioControls), RejoinEnd> {
        let started = std::time::Instant::now();
        let mut backoff = ReconnectBackoff::new(ReconnectPolicy::rejoin(), started);
        let max_attempts = backoff.policy().max_attempts;
        let deadline = tokio::time::Instant::from_std(backoff.deadline());
        let waiting = self.reconnecting(last, CallInterruption::WaitingForNetwork);
        // Confirm the old departure before any fresh membership is published,
        // so peers never see two joins from this device.
        if !signaling_ready(inputs.sync) {
            self.publish_recovery(waiting.clone()).await;
            if !self.recovery_pause(inputs, deadline, true).await? {
                let error = rejoin_exhausted(cause, 0, None);
                return Err(RejoinEnd::Stopped(Err(error)));
            }
        }
        let first = CallInterruption::Rejoining {
            attempt: 1,
            max_attempts,
        };
        let starting = self.reconnecting(last, first);
        self.publish_recovery(starting).await;
        let left = {
            let leave = leave_with_timeout(rtc, &self.room_id);
            tokio::pin!(leave);
            loop {
                tokio::select! {
                    result = &mut leave => break result,
                    event = inputs.outgoing.next_event() => {
                        inputs.outgoing.handle_detached_event(event).await;
                    }
                }
            }
        };
        match left {
            Ok(()) | Err(CallControllerError::Rtc(RtcError::NotJoined)) => {}
            Err(error) => {
                return Err(RejoinEnd::CleanupFailed(format!(
                    "{}; leaving the interrupted join failed: {error}",
                    cause.message()
                )));
            }
        }
        let mut last_error = None;
        loop {
            let Some(delay) = backoff.next_attempt(std::time::Instant::now()) else {
                let error = rejoin_exhausted(cause, backoff.attempts(), last_error);
                return Err(RejoinEnd::Stopped(Err(error)));
            };
            let interruption = CallInterruption::Rejoining {
                attempt: backoff.attempts(),
                max_attempts,
            };
            let rejoining = self.reconnecting(last, interruption);
            self.publish_recovery(rejoining.clone()).await;
            let attempt_at = tokio::time::Instant::now() + delay;
            self.recovery_pause(inputs, attempt_at, false).await?;
            if !signaling_ready(inputs.sync) {
                self.publish_recovery(waiting.clone()).await;
                if !self.recovery_pause(inputs, deadline, true).await? {
                    let attempts = backoff.attempts().saturating_sub(1);
                    let error = rejoin_exhausted(cause, attempts, last_error);
                    return Err(RejoinEnd::Stopped(Err(error)));
                }
                self.publish_recovery(rejoining).await;
            }
            // Handling a Stop or waiting for signaling can outlast the
            // nominal backoff. Recheck at the actual join boundary and cap
            // this attempt to the remaining wall-clock recovery budget.
            let remaining = backoff.remaining(std::time::Instant::now());
            if remaining.is_zero() {
                let error =
                    rejoin_exhausted(cause, backoff.attempts().saturating_sub(1), last_error);
                return Err(RejoinEnd::Stopped(Err(error)));
            }
            match self.join_attempt(rtc, inputs.outgoing, remaining).await {
                Ok(info) => return self.install_join(rtc, info, requested, inputs).await,
                Err(JoinAttemptError::Stopped(end)) => return Err(end),
                Err(JoinAttemptError::Failed(CallControllerError::Cleanup(reason))) => {
                    return Err(RejoinEnd::CleanupFailed(reason));
                }
                Err(JoinAttemptError::Failed(error)) if rejoin_retryable(&error) => {
                    last_error = Some(error);
                }
                Err(JoinAttemptError::Failed(terminal)) => {
                    return Err(RejoinEnd::Stopped(Err(terminal)));
                }
            }
        }
    }

    /// Bind fresh audio controls, carrying the requested mute and deafen
    /// state, to the new join and swap them in unless Leave or an account
    /// change won meanwhile; the new join is left again in that case.
    async fn install_join(
        &mut self,
        rtc: &mut RtcService,
        info: CallInfo,
        requested: AudioState,
        inputs: &mut RecoveryInputs<'_>,
    ) -> Result<(CallInfo, AudioControls), RejoinEnd> {
        let controls = match self.new_audio_controls(rtc, &info, None, inputs.sync) {
            Ok(controls) => controls,
            Err(error) => return Err(leave_after(rtc, &self.room_id, Err(error.into())).await),
        };
        if let Err(error) = controls.preset_requested_state(requested) {
            return Err(leave_after(rtc, &self.room_id, Err(error.into())).await);
        }
        let installed = {
            let mut slot = self.shared.slot.lock().await;
            match &mut *slot {
                CallSlot::Running(running)
                    if running.token == self.token
                        && running.presentation == self.presentation
                        && running.cancel.is_some()
                        && self.selection.is_current()
                        && !self.capture_stop.requested() =>
                {
                    running.audio_controls = Some(controls.clone());
                    self.shared.status.send_replace(CallStatus::Joined {
                        account_id: self.account_id.clone(),
                        call: info.clone(),
                    });
                    true
                }
                _ => false,
            }
        };
        if !installed {
            return Err(leave_after(rtc, &self.room_id, self.stop_result()).await);
        }
        Ok((info, controls))
    }

    /// Return true if the caller waiting for startup has gone away.
    fn send_ready(&mut self, result: Result<CallInfo, CallControllerError>) -> bool {
        self.ready
            .take()
            .is_none_or(|sender| sender.send(result).is_err())
    }

    async fn finish(&mut self, cleanup: Result<(), String>, failure: Option<String>) {
        let outcome = failure.map_or(CallOutcome::Idle, CallOutcome::Failed);
        self.finish_with(cleanup, outcome).await;
    }

    async fn finish_with(&mut self, cleanup: Result<(), String>, outcome: CallOutcome) {
        // Revoke the old media generation before waiting for its native owner.
        self.shared.clear_media(self.token, self.presentation);
        // A failed earlier cleanup already retains this lease and its original
        // error. Successful call cleanup additionally requires actual opener
        // completion; a timeout cannot publish Idle or release update exclusion.
        let cleanup = match cleanup {
            Ok(()) => match morpheus_rtc::wait_for_audio_device_release().await {
                Ok(()) => morpheus_rtc::wait_for_ring_audio_release()
                    .await
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            },
            Err(original) => Err(original),
        };
        {
            let mut slot = self.shared.slot.lock().await;
            let done = match &*slot {
                CallSlot::Running(running) if running.token == self.token => {
                    Some(running.done.clone())
                }
                CallSlot::CleanupFailed { token, done } if *token == self.token => {
                    Some(done.clone())
                }
                _ => None,
            };
            if let Some(done) = done {
                if let Err(reason) = &cleanup {
                    *slot = CallSlot::CleanupFailed {
                        token: self.token,
                        done,
                    };
                    self.shared.status.send_replace(CallStatus::CleanupFailed {
                        account_id: self.account_id.clone(),
                        room_id: self.room_id.clone(),
                        reason: reason.clone(),
                    });
                } else {
                    *slot = CallSlot::Idle;
                    self.shared.release_update_lease();
                    self.shared.status.send_replace(match outcome {
                        CallOutcome::Idle => CallStatus::Idle,
                        CallOutcome::Failed(reason) => CallStatus::Failed {
                            room_id: self.room_id.clone(),
                            reason,
                        },
                        CallOutcome::Ended(reason) => CallStatus::Ended {
                            room_id: self.room_id.clone(),
                            reason,
                        },
                    });
                }
            }
            // Publish completion while holding the slot lock, so a later
            // acknowledgement sees a completed first cleanup attempt.
            self.done.send_replace(Some(cleanup));
        }
    }
}

type LocalAudioRun<'a> = Pin<Box<dyn Future<Output = Result<(), MediaError>> + Send + 'a>>;

/// A join's local audio loop on `chosen`, resuming where a lost join (or a
/// stopped loop) left it (`resume`); `retry` when the request that started
/// it was the user's unchanged repeat. It owns its controls, so audio can
/// start again on new controls inside the same join.
fn run_local_audio<'a>(
    rtc: &'a RtcService,
    room_id: &'a str,
    controls: AudioControls,
    resume: Option<AudioResume>,
    chosen: AudioDeviceSelection,
    retry: bool,
) -> LocalAudioRun<'a> {
    Box::pin(async move {
        rtc.run_audio_resuming(room_id, &controls, resume, chosen, retry)
            .await
    })
}

/// Run a "Use for this call" admission for call generation `token` within
/// `limit` (`ROUTE_ADMISSION_TIMEOUT`), and abandon it as soon as that
/// generation is no longer the one on screen (Leave, an end, a rejoin, an
/// account switch). The request is queued only by `admission` completing; on
/// a timeout or a retirement nothing was queued, and nothing is retried.
async fn bounded_route_admission(
    shared: &Shared,
    mut status: watch::Receiver<CallStatus>,
    token: u64,
    limit: Duration,
    admission: impl Future<Output = Result<(), CallControllerError>>,
) -> Result<(), CallControllerError> {
    let retired = async {
        loop {
            if shared.active_video_token.load(Ordering::SeqCst) != token
                || !status.borrow_and_update().controls_available()
            {
                return;
            }
            if status.changed().await.is_err() {
                return;
            }
        }
    };
    tokio::select! {
        biased;
        () = retired => Err(CallControllerError::StaleCall),
        admitted = timeout(limit, admission) => admitted
            .unwrap_or(Err(CallControllerError::TimedOut("switching call audio"))),
    }
}

/// Queue `selection` for generation `token` of the running call, after a
/// final check under the slot lock that the call, its generation and its
/// account (`account_id`) still own it. The send is the last step, after the
/// only await, so a cancelled admission never queues anything.
async fn admit_route_request(
    shared: &Shared,
    token: u64,
    account_id: &str,
    selection: AudioDeviceSelection,
) -> Result<(), CallControllerError> {
    let slot = shared.slot.lock().await;
    let CallSlot::Running(running) = &*slot else {
        return Err(CallControllerError::StaleCall);
    };
    if running.presentation != token
        || shared.active_video_token.load(Ordering::SeqCst) != token
        || running.cancel.is_none()
        || running.capture_stop.requested()
        || !shared.status.borrow().controls_available()
    {
        return Err(CallControllerError::StaleCall);
    }
    if running.account_id != account_id {
        return Err(CallControllerError::AccountChanged);
    }
    // An unchanged repeat of the route this generation runs with (what its
    // join opened, or the last request admitted for it) is a retry. Each
    // admission moves the baseline, so a change and its undo never are.
    let retry = {
        let mut baseline = shared
            .route_baseline
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let retry = baseline.0 == token && baseline.1 == selection;
        *baseline = (token, selection.clone());
        retry
    };
    shared.route_requests.send_replace(RouteRequest {
        token,
        selection,
        retry,
    });
    Ok(())
}

/// Where the rejoin that replaces a lost join resumes local audio: the lost
/// join's own point (recorded before its track was even published), else
/// the point it was itself resumed from, else, when its audio never opened
/// a device, a conservative point for `chosen` that reopens a microphone
/// only by an exact chosen ID. An automatic rejoin therefore never opens a
/// System default or alias microphone whose identity it does not hold.
fn rejoin_resume_point(
    lost_join: Option<AudioResume>,
    carried: Option<AudioResume>,
    chosen: &AudioDeviceSelection,
) -> AudioResume {
    lost_join
        .or(carried)
        .unwrap_or_else(|| AudioResume::unestablished(chosen))
}

/// The route request queued for generation `presentation`, read and marked
/// seen. Reading the current value, not only a change, matters right after
/// subscribing: Joined is published first, so a choice accepted in between
/// is already the receiver's seen value.
fn take_route_request(
    requests: &mut watch::Receiver<RouteRequest>,
    presentation: u64,
) -> Option<RouteRequest> {
    let request = requests.borrow_and_update().clone();
    (request.token == presentation).then_some(request)
}

/// At the start of each join of generation `presentation`: take a request
/// already queued for it (Joined is published before the worker subscribes,
/// and requests admitted while it rejoined wait too) into `chosen`, and
/// record `chosen` as the route baseline admissions compare with. Under the
/// slot lock, so no admission interleaves. Returns whether the taken request
/// was the user's unchanged repeat.
async fn begin_session_route(
    shared: &Shared,
    requests: &mut watch::Receiver<RouteRequest>,
    presentation: u64,
    chosen: &mut AudioDeviceSelection,
) -> bool {
    let _slot = shared.slot.lock().await;
    let mut retry = false;
    if let Some(request) = take_route_request(requests, presentation) {
        *chosen = request.selection;
        retry = request.retry;
    }
    *shared
        .route_baseline
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = (presentation, chosen.clone());
    retry
}

/// Install restarted audio `controls` only while generation `presentation`
/// of call `token` is still the running one for its account; Leave or an
/// account change meanwhile cancels them instead. A free function over the
/// worker's Sync parts, so the worker future stays Send.
async fn install_restarted_audio(
    shared: &Shared,
    token: u64,
    presentation: u64,
    selection: &AccountSelection,
    capture_stop: &CaptureStop,
    controls: AudioControls,
) -> Option<AudioControls> {
    let mut slot = shared.slot.lock().await;
    match &mut *slot {
        CallSlot::Running(running)
            if running.token == token
                && running.presentation == presentation
                && running.cancel.is_some()
                && selection.is_current()
                && !capture_stop.requested() =>
        {
            running.audio_controls = Some(controls.clone());
            shared.rearm_audio(presentation);
            Some(controls)
        }
        _ => {
            controls.cancel();
            None
        }
    }
}

async fn leave_with_timeout(
    rtc: &mut RtcService,
    room_id: &str,
) -> Result<(), CallControllerError> {
    timeout(LEAVE_TIMEOUT, rtc.leave(room_id))
        .await
        .map_err(|_| CallControllerError::TimedOut("leaving MatrixRTC call"))??;
    Ok(())
}

async fn cleanup_after_failed_join(
    rtc: &mut RtcService,
    room_id: &str,
    original: CallControllerError,
) -> CallControllerError {
    // The upstream join can publish membership before it yields a call handle.
    // Try an explicit leave even after an error or timeout. If the handle was
    // never installed, NotJoined is expected and the server's delayed leave
    // remains the fallback for a membership published by an interrupted join.
    match leave_with_timeout(rtc, room_id).await {
        Ok(()) | Err(CallControllerError::Rtc(RtcError::NotJoined)) => original,
        Err(cleanup) => CallControllerError::Cleanup(format!(
            "{original}; cleanup after failed join also failed: {cleanup}"
        )),
    }
}

async fn ensure_account(
    manager: &AccountManager,
    account_id: &str,
) -> Result<(), CallControllerError> {
    if manager
        .active_account()
        .await
        .as_ref()
        .map(|account| account.id.as_str())
        == Some(account_id)
    {
        Ok(())
    } else {
        Err(CallControllerError::AccountChanged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn attachment_audio_authority_ends_before_a_queued_or_opening_call_and_never_revives() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let controller = CallController::new(manager, UpdateActivity::default());
        let old = controller.attachment_audio_authority();
        assert!(old());
        let queued = controller.reserve_ringing_request().unwrap();
        assert!(!old());
        drop(queued);
        assert!(!old(), "completed request must not revive old audio");
        let next = controller.attachment_audio_authority();
        assert!(next());
        let (token, done) = controller
            .shared
            .reserve_open_slot("fixture", "!fixture:example.org")
            .await
            .unwrap();
        assert!(
            !next(),
            "native media has not opened but the reservation already excludes attachment output"
        );
        controller.shared.finish_open_slot(token).await.unwrap();
        done.send_replace(true);
        assert!(!next(), "call completion must not adopt the old lease");
        assert!((controller.attachment_audio_authority())());
    }

    #[test]
    fn worker_future_remains_send_with_owned_incoming_offer() {
        fn require_send<T: Send>(_: T) {}
        // Type-check the real future without creating accounts or a runtime.
        let _check: fn(CallWorker) = |worker| require_send(worker.run());
    }

    #[tokio::test]
    async fn revoked_start_does_not_poll_the_join_operation() {
        let entered = AtomicBool::new(false);
        let notified = AtomicBool::new(false);
        let result = settle_owned_join(
            async {
                entered.store(true, Ordering::SeqCst);
                7
            },
            async {},
            || {
                notified.store(true, Ordering::SeqCst);
            },
        )
        .await;
        assert_eq!(result, (None, true));
        assert!(!entered.load(Ordering::SeqCst));
        assert!(notified.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancelled_inflight_join_is_retained_for_returned_handle_cleanup() {
        let (cancel, cancelled) = oneshot::channel();
        let (complete, joined) = oneshot::channel();
        let entered = AtomicBool::new(false);
        let notified = AtomicBool::new(false);
        let mut operation = Box::pin(settle_owned_join(
            async {
                entered.store(true, Ordering::SeqCst);
                joined.await.unwrap()
            },
            async {
                let _ = cancelled.await;
            },
            || {
                notified.store(true, Ordering::SeqCst);
            },
        ));
        assert!(futures_util::poll!(operation.as_mut()).is_pending());
        assert!(entered.load(Ordering::SeqCst));
        cancel.send(()).unwrap();
        assert!(futures_util::poll!(operation.as_mut()).is_pending());
        assert!(notified.load(Ordering::SeqCst));
        // Cancellation did not discard the receiver/possible call handle.
        complete.send("owned-call-handle").unwrap();
        assert_eq!(operation.await, (Some("owned-call-handle"), true));
    }

    #[tokio::test]
    async fn retained_join_keeps_its_existing_timeout_after_cancellation() {
        let (cancel, cancelled) = oneshot::channel();
        let (elapsed, deadline) = oneshot::channel();
        // An injected existing deadline models the caller-owned timeout;
        // cancellation must neither replace it nor re-run a failed join.
        let mut operation = Box::pin(settle_owned_join(
            async {
                deadline.await.unwrap();
                Err::<(), _>("join-timeout")
            },
            async {
                let _ = cancelled.await;
            },
            || {},
        ));
        assert!(futures_util::poll!(operation.as_mut()).is_pending());
        cancel.send(()).unwrap();
        assert!(futures_util::poll!(operation.as_mut()).is_pending());
        elapsed.send(()).unwrap();
        assert_eq!(operation.await, (Some(Err("join-timeout")), true));
    }

    #[tokio::test]
    async fn successful_join_does_not_report_cancellation() {
        let result =
            settle_owned_join(async { 7 }, pending(), || panic!("unexpected cancellation")).await;
        assert_eq!(result, (Some(7), false));
    }

    #[test]
    fn audio_permission_reads_stop_selection_and_sync_without_watcher_progress() {
        let stop = CaptureStop::default();
        let (sync, current) = watch::channel(SyncStatus::Running {
            backend: SyncBackend::SlidingSticky,
        });
        assert!(audio_capture_allowed(&stop, true, &current.borrow()));
        assert!(!audio_capture_allowed(&stop, false, &current.borrow()));
        sync.send_replace(SyncStatus::Stopped);
        assert!(!audio_capture_allowed(&stop, true, &current.borrow()));
        // A sticky-less fallback ends authorization; a retry only pauses it.
        sync.send_replace(SyncStatus::Running {
            backend: SyncBackend::Classic,
        });
        assert!(!audio_capture_allowed(&stop, true, &current.borrow()));
        sync.send_replace(SyncStatus::Retrying {
            after_seconds: 2,
            reason: "offline".into(),
        });
        assert!(audio_capture_allowed(&stop, true, &current.borrow()));
        sync.send_replace(SyncStatus::Running {
            backend: SyncBackend::SlidingSticky,
        });
        stop.request();
        assert!(!audio_capture_allowed(&stop, true, &current.borrow()));
    }

    #[tokio::test]
    async fn opening_slot_blocks_account_switch_until_write_finishes() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let controller = Arc::new(CallController::new(manager, UpdateActivity::default()));
        let (token, done) = controller
            .shared
            .reserve_open_slot("account-a", "!room-a:example.org")
            .await
            .unwrap();
        assert!(controller.status().blocks_account_switch());
        assert!(!controller.ringing_allowed(controller.ringing_generation()));
        assert!(matches!(
            controller.shared.update_activity.try_install(),
            Err(UpdateError::CallActive)
        ));
        assert!(matches!(
            controller.status(),
            CallStatus::OpeningSlot { room_id, .. } if room_id == "!room-a:example.org"
        ));
        let waiting = {
            let controller = controller.clone();
            tokio::spawn(async move { controller.end_call().await })
        };
        tokio::task::yield_now().await;
        assert!(controller.status().blocks_account_switch());
        assert!(!waiting.is_finished());
        controller.shared.finish_open_slot(token).await.unwrap();
        done.send_replace(true);
        waiting.await.unwrap().unwrap();
        assert!(!controller.status().blocks_account_switch());
        assert!(controller.shared.update_activity.try_install().is_ok());
    }

    #[tokio::test]
    async fn opening_slot_keeps_update_exclusion_until_real_ring_receipt_and_reports_failed_drain()
    {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let activity = UpdateActivity::default();
        let controller = Arc::new(CallController::new(manager, activity.clone()));
        let (token, done) = controller
            .shared
            .reserve_open_slot("fixture", "!fixture:example.org")
            .await
            .unwrap();
        let waiting = {
            let controller = controller.clone();
            tokio::spawn(async move { controller.end_call().await })
        };
        tokio::task::yield_now().await;
        let (release, released) = oneshot::channel();
        let mut cleanup = Box::pin(
            controller
                .shared
                .finish_open_slot_after_ring(token, async { released.await.unwrap() }),
        );
        assert!(futures_util::poll!(cleanup.as_mut()).is_pending());
        assert!(matches!(
            activity.try_install(),
            Err(UpdateError::CallActive)
        ));
        assert!(controller.status().blocks_account_switch());
        release
            .send(Err(
                morpheus_rtc::RingAudioReleaseError::OutputOwnerUnconfirmed,
            ))
            .unwrap();
        assert!(matches!(
            cleanup.await,
            Err(CallControllerError::Cleanup(_))
        ));
        done.send_replace(true);
        assert!(matches!(
            waiting.await.unwrap(),
            Err(CallControllerError::Cleanup(_))
        ));
        assert!(
            matches!(*controller.shared.slot.lock().await, CallSlot::CleanupFailed { token: current, .. } if current == token)
        );
        assert!(matches!(
            activity.try_install(),
            Err(UpdateError::CallActive)
        ));
        assert!(matches!(
            controller
                .shared
                .reserve_open_slot("fixture", "!next:example.org")
                .await,
            Err(CallControllerError::AlreadyBusy)
        ));
        // The existing explicit warning path still requires an independently
        // joined ring. This synthetic case created no RTC owner, so it can clear.
        controller.abandon_failed_leave().await.unwrap();
        assert!(activity.try_install().is_ok());
    }

    #[tokio::test]
    async fn update_reservation_blocks_room_state_writes_and_stale_completion_cannot_unlock() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let activity = UpdateActivity::default();
        let controller = CallController::new(manager, activity.clone());
        let installation = activity.try_install().unwrap();
        assert!(matches!(
            controller
                .shared
                .reserve_open_slot("account", "!room:example.org")
                .await,
            Err(CallControllerError::Update(UpdateError::UpdateActive))
        ));
        assert!(matches!(controller.status(), CallStatus::Idle));
        drop(installation);
        let (token, _done) = controller
            .shared
            .reserve_open_slot("account", "!room:example.org")
            .await
            .unwrap();
        controller.shared.finish_open_slot(token + 1).await.unwrap();
        assert!(matches!(
            activity.try_install(),
            Err(UpdateError::CallActive)
        ));
        controller.shared.finish_open_slot(token).await.unwrap();
        assert!(activity.try_install().is_ok());
    }

    #[tokio::test]
    async fn failed_leave_requires_distinct_acknowledgement_after_worker_terminates() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let controller = Arc::new(CallController::new(manager, UpdateActivity::default()));
        let (done_tx, done_rx) = watch::channel(None);
        {
            let mut slot = controller.shared.slot.lock().await;
            controller.shared.reserve_update_lease().unwrap();
            *slot = CallSlot::CleanupFailed {
                token: 7,
                done: done_rx,
            };
            controller
                .shared
                .status
                .send_replace(CallStatus::CleanupFailed {
                    account_id: "account-a".into(),
                    room_id: "!room-a:example.org".into(),
                    reason: "leave status unknown".into(),
                });
        }
        let waiting = {
            let controller = controller.clone();
            tokio::spawn(async move { controller.end_call().await })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        assert!(controller.status().blocks_account_switch());
        done_tx.send_replace(Some(Err("server leave failed".into())));
        assert!(matches!(
            waiting.await.unwrap(),
            Err(CallControllerError::Cleanup(reason)) if reason == "server leave failed"
        ));
        assert!(controller.status().blocks_account_switch());
        assert!(matches!(
            controller.shared.update_activity.try_install(),
            Err(UpdateError::CallActive)
        ));
        controller.abandon_failed_leave().await.unwrap();
        assert!(!controller.status().blocks_account_switch());
        assert!(controller.shared.update_activity.try_install().is_ok());
    }

    #[tokio::test]
    async fn acknowledgement_cannot_release_update_exclusion_without_actual_opener_receipt() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let activity = UpdateActivity::default();
        let controller = CallController::new(manager, activity.clone());
        let (_done, done) = watch::channel(Some(Err("server leave failed".into())));
        controller.shared.reserve_update_lease().unwrap();
        *controller.shared.slot.lock().await = CallSlot::CleanupFailed { token: 7, done };
        controller
            .shared
            .status
            .send_replace(CallStatus::CleanupFailed {
                account_id: "account-a".into(),
                room_id: "!room-a:example.org".into(),
                reason: "server leave failed".into(),
            });
        let (release, released) = oneshot::channel();
        let mut acknowledgement = Box::pin(
            controller.abandon_failed_leave_after_opener(async { released.await.unwrap() }),
        );
        assert!(futures_util::poll!(acknowledgement.as_mut()).is_pending());
        assert!(matches!(
            activity.try_install(),
            Err(UpdateError::CallActive)
        ));
        // Dropping the UI's wait cannot publish Idle or release the existing
        // lease. The RTC owner is independently retained, never in this future.
        drop(acknowledgement);
        drop(release);
        assert!(controller.status().blocks_account_switch());
        assert!(matches!(
            activity.try_install(),
            Err(UpdateError::CallActive)
        ));
        for error in [
            morpheus_rtc::AudioDeviceReleaseError::TimedOut,
            morpheus_rtc::AudioDeviceReleaseError::WorkerPanicked,
            morpheus_rtc::AudioDeviceReleaseError::SupervisorFailed,
            #[cfg(target_os = "macos")]
            morpheus_rtc::AudioDeviceReleaseError::NativeClose,
        ] {
            assert!(matches!(
                controller.abandon_failed_leave_after_opener(async { Err(error) }).await,
                Err(CallControllerError::Cleanup(reason)) if reason == error.to_string()
            ));
            assert!(controller.status().blocks_account_switch());
            assert!(matches!(
                activity.try_install(),
                Err(UpdateError::CallActive)
            ));
        }
        // The server-leave warning is still an explicit acknowledgement: only
        // actual local completion allows its existing path to clear the lease.
        controller
            .abandon_failed_leave_after_opener(async { Ok(()) })
            .await
            .unwrap();
        assert!(matches!(controller.status(), CallStatus::Idle));
        assert!(activity.try_install().is_ok());
    }

    #[tokio::test]
    async fn acknowledgement_waits_for_worker_completion() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let controller = Arc::new(CallController::new(manager, UpdateActivity::default()));
        let (done_tx, done_rx) = watch::channel(None);
        {
            let mut slot = controller.shared.slot.lock().await;
            *slot = CallSlot::CleanupFailed {
                token: 7,
                done: done_rx,
            };
            controller
                .shared
                .status
                .send_replace(CallStatus::CleanupFailed {
                    account_id: "account-a".into(),
                    room_id: "!room-a:example.org".into(),
                    reason: "leave status unknown".into(),
                });
        }
        let waiting = {
            let controller = controller.clone();
            tokio::spawn(async move { controller.abandon_failed_leave().await })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        assert!(controller.status().blocks_account_switch());
        done_tx.send_replace(Some(Err("server leave failed".into())));
        waiting.await.unwrap().unwrap();
        assert!(!controller.status().blocks_account_switch());
    }

    #[tokio::test]
    async fn stuck_capture_worker_keeps_account_switch_blocked() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let controller = CallController::new(manager, UpdateActivity::default());
        let (_, done_rx) = watch::channel(Some(Err("native capture still stopping".into())));
        {
            let mut slot = controller.shared.slot.lock().await;
            *slot = CallSlot::CleanupFailed {
                token: 7,
                done: done_rx,
            };
            controller
                .shared
                .status
                .send_replace(CallStatus::CleanupFailed {
                    account_id: "account-a".into(),
                    room_id: "!room-a:example.org".into(),
                    reason: "native capture still stopping".into(),
                });
        }
        controller
            .shared
            .active_capture_workers
            .store(1, Ordering::SeqCst);
        assert!(matches!(
            controller.abandon_failed_leave().await,
            Err(CallControllerError::Cleanup(_))
        ));
        assert!(controller.status().blocks_account_switch());
        controller
            .shared
            .active_capture_workers
            .store(0, Ordering::SeqCst);
        controller.abandon_failed_leave().await.unwrap();
        assert!(!controller.status().blocks_account_switch());
    }

    #[test]
    fn recovering_calls_keep_blocking_account_switches() {
        let call = CallInfo {
            room_id: "!room:example.org".into(),
            slot_id: "m.call#room".into(),
            membership_id: "member".into(),
            local_identity: "identity".into(),
        };
        for interruption in [
            CallInterruption::Media,
            CallInterruption::Signaling,
            CallInterruption::WaitingForNetwork,
            CallInterruption::Rejoining {
                attempt: 2,
                max_attempts: 4,
            },
        ] {
            let status = CallStatus::Reconnecting {
                account_id: "account-a".into(),
                call: call.clone(),
                interruption,
            };
            assert!(status.blocks_account_switch());
            assert!(status.controls_available());
        }
        let ended = CallStatus::Ended {
            room_id: call.room_id,
            reason: "the room call was closed".into(),
        };
        assert!(!ended.blocks_account_switch());
        assert!(!ended.controls_available());
    }

    #[tokio::test]
    async fn late_recovery_status_cannot_overwrite_a_leave() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let controller = CallController::new(manager, UpdateActivity::default());
        let shared = Arc::clone(&controller.shared);
        let (cancel, _cancelled) = oneshot::channel();
        let (_done, done) = watch::channel(None);
        let (video_commands, _video_rx) = mpsc::channel(1);
        let stop = Arc::new(CaptureStop::default());
        *shared.slot.lock().await = CallSlot::Running(RunningCall {
            token: 7,
            presentation: 9,
            account_id: "account-a".into(),
            room_id: "!room:example.org".into(),
            cancel: Some(cancel),
            done,
            audio_controls: None,
            video_commands,
            capture_stop: Arc::clone(&stop),
        });
        let next = CallStatus::Reconnecting {
            account_id: "account-a".into(),
            call: CallInfo {
                room_id: "!room:example.org".into(),
                slot_id: "m.call#room".into(),
                membership_id: "member".into(),
                local_identity: "identity".into(),
            },
            interruption: CallInterruption::WaitingForNetwork,
        };
        shared.publish_live_status(7, &stop, next.clone()).await;
        assert_eq!(controller.status(), next);
        // Another call generation cannot publish for this slot.
        shared.publish_live_status(8, &stop, CallStatus::Idle).await;
        assert_eq!(controller.status(), next);
        // Leave takes the cancel sender, so a later recovery status is dropped.
        let ending = CallStatus::Ending {
            account_id: "account-a".into(),
            room_id: "!room:example.org".into(),
        };
        {
            let mut slot = shared.slot.lock().await;
            let CallSlot::Running(running) = &mut *slot else {
                unreachable!()
            };
            running.cancel.take();
            shared.status.send_replace(ending.clone());
        }
        shared.publish_live_status(7, &stop, next).await;
        assert_eq!(controller.status(), ending);
    }

    #[test]
    fn call_modes_keep_current_slot_and_legacy_room_state_separate() {
        use morpheus_rtc::InteropMode;

        let standard = CallJoinMode::Standard.options();
        assert_eq!(standard.interop_mode, InteropMode::Current);
        assert_eq!(standard.slot_id, "m.call#room");

        let sticky =
            CallJoinMode::from(morpheus_config::RoomCallMode::ElementCallMatrix2).options();
        assert_eq!(sticky, CallOptions::element_call_matrix_2_0());
        assert_eq!(sticky.interop_mode, InteropMode::Sticky2025);

        let legacy = CallJoinMode::ElementCallRoomState.options();
        assert_eq!(legacy.interop_mode, InteropMode::RoomState);
        assert_eq!(legacy.slot_id, "m.call#room");
    }

    #[tokio::test]
    async fn stale_generation_cannot_start_native_capture() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let controller = CallController::new(manager, UpdateActivity::default());
        controller
            .shared
            .active_video_token
            .store(9, Ordering::SeqCst);
        let result = controller
            .start_video_capture(
                8,
                CaptureSource::Camera("n:0:stale".into()),
                "Camera".into(),
            )
            .await;
        assert!(matches!(result, Err(CallControllerError::StaleCall)));
    }

    #[test]
    fn final_cleanup_reasserts_clear_after_late_old_call_update() {
        let (status, _) = watch::channel(CallStatus::Idle);
        let (video, _) = watch::channel(VideoUpdate::Clear { token: 5 });
        let (roster, _) = watch::channel(RosterUpdate {
            token: 5,
            members: Vec::new(),
        });
        let (audio, _) = watch::channel(AudioUpdate::Unavailable { token: 5 });
        let (local_video, _) = watch::channel(LocalVideoStatus {
            token: 5,
            ..LocalVideoStatus::default()
        });
        let shared = Shared {
            ring_request: Arc::new(RingRequestGate::default()),
            slot: Mutex::new(CallSlot::Idle),
            update_activity: UpdateActivity::default(),
            update_call_lease: std::sync::Mutex::new(None),
            status,
            invitation: watch::channel(InvitationUpdate::default()).0,
            video,
            roster,
            stage: watch::channel(StageRequest::default()).0,
            audio,
            audio_route: watch::channel(AudioRouteUpdate::default()).0,
            route_requests: watch::channel(RouteRequest::default()).0,
            route_baseline: std::sync::Mutex::default(),
            local_video,
            active_capture_workers: Arc::new(AtomicUsize::new(0)),
            active_video_token: AtomicU64::new(5),
            active_audio_token: AtomicU64::new(5),
            active_call_token: AtomicU64::new(5),
            next_token: AtomicU64::new(6),
            #[cfg(all(test, target_os = "linux"))]
            recovery_keys: broadcast::channel(64).0,
        };
        shared.invitation.send_replace(InvitationUpdate {
            token: 5,
            text: "Invitation sent",
        });
        shared.clear_media(5, 5);
        assert_eq!(shared.active_video_token.load(Ordering::SeqCst), 0);
        assert_eq!(*shared.invitation.borrow(), InvitationUpdate::default());
        shared.invitation.send_replace(InvitationUpdate {
            token: 5,
            text: "Late invitation",
        });
        shared.video.send_replace(VideoUpdate::Clear { token: 5 });
        shared.local_video.send_replace(LocalVideoStatus {
            token: 5,
            camera: crate::local_video::CaptureState {
                phase: crate::local_video::CapturePhase::Active,
                source: "Old camera".into(),
            },
            ..LocalVideoStatus::default()
        });
        shared.roster.send_replace(RosterUpdate {
            token: 5,
            members: Vec::new(),
        });
        shared.clear_media(5, 5);
        assert_eq!(*shared.invitation.borrow(), InvitationUpdate::default());
        assert_eq!(shared.video.borrow().token(), 0);
        assert_eq!(shared.roster.borrow().token, 0);
        assert_eq!(shared.local_video.borrow().token, 0);

        shared.active_video_token.store(6, Ordering::SeqCst);
        let current_invitation = InvitationUpdate {
            token: 6,
            text: "Current invitation",
        };
        shared.invitation.send_replace(current_invitation);
        shared.video.send_replace(VideoUpdate::Clear { token: 6 });
        shared.local_video.send_replace(LocalVideoStatus {
            token: 6,
            screen: crate::local_video::CaptureState {
                phase: crate::local_video::CapturePhase::Active,
                source: "New display".into(),
            },
            ..LocalVideoStatus::default()
        });
        shared.clear_media(5, 5);
        assert_eq!(*shared.invitation.borrow(), current_invitation);
        assert_eq!(shared.video.borrow().token(), 6);
        assert_eq!(shared.local_video.borrow().token, 6);
        assert_eq!(shared.local_video.borrow().screen.source, "New display");

        // End A can win its video CAS, then be descheduled before retiring
        // its lifecycle identity. Its worker can finish A and install B in
        // between. Resume A at that exact remaining production step: it must
        // not erase B's identity, even when B has a different join generation.
        shared.active_call_token.store(6, Ordering::SeqCst);
        shared.active_video_token.store(9, Ordering::SeqCst);
        shared.clear_call_identity(5);
        assert_eq!(shared.active_call_token.load(Ordering::SeqCst), 6);
        assert_eq!(shared.active_video_token.load(Ordering::SeqCst), 9);
        shared.clear_media(5, 5);
        assert_eq!(shared.active_call_token.load(Ordering::SeqCst), 6);

        // A rejoined call is retired by its stable call ID, not by the
        // presentation number. Clearing that last join retires only B.
        shared.clear_media(6, 9);
        assert_eq!(shared.active_call_token.load(Ordering::SeqCst), 0);
        assert_eq!(shared.active_video_token.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn audio_updates_are_scoped_to_the_current_call_generation() {
        let (status, _) = watch::channel(CallStatus::Idle);
        let (video, _) = watch::channel(VideoUpdate::Clear { token: 5 });
        let (roster, _) = watch::channel(RosterUpdate {
            token: 5,
            members: Vec::new(),
        });
        let (audio, _) = watch::channel(AudioUpdate::Unavailable { token: 5 });
        let (local_video, _) = watch::channel(LocalVideoStatus {
            token: 5,
            ..LocalVideoStatus::default()
        });
        let shared = Shared {
            ring_request: Arc::new(RingRequestGate::default()),
            slot: Mutex::new(CallSlot::Idle),
            update_activity: UpdateActivity::default(),
            update_call_lease: std::sync::Mutex::new(None),
            status,
            invitation: watch::channel(InvitationUpdate::default()).0,
            video,
            roster,
            stage: watch::channel(StageRequest::default()).0,
            audio,
            audio_route: watch::channel(AudioRouteUpdate::default()).0,
            route_requests: watch::channel(RouteRequest::default()).0,
            route_baseline: std::sync::Mutex::default(),
            local_video,
            active_capture_workers: Arc::new(AtomicUsize::new(0)),
            active_video_token: AtomicU64::new(5),
            active_audio_token: AtomicU64::new(5),
            active_call_token: AtomicU64::new(5),
            next_token: AtomicU64::new(6),
            #[cfg(all(test, target_os = "linux"))]
            recovery_keys: broadcast::channel(64).0,
        };
        let state = AudioState {
            microphone_muted: true,
            deafened: true,
        };
        shared.send_applied_audio(5, state);
        assert_eq!(
            *shared.audio.borrow(),
            AudioUpdate::Applied { token: 5, state }
        );

        shared.clear_audio(5);
        shared.send_applied_audio(5, state);
        assert_eq!(
            *shared.audio.borrow(),
            AudioUpdate::Unavailable { token: 0 }
        );
        // A producer that already passed its token check may send late; final
        // worker cleanup must publish Unavailable again for this generation.
        shared
            .audio
            .send_replace(AudioUpdate::Applied { token: 5, state });
        shared.clear_audio(5);
        assert_eq!(
            *shared.audio.borrow(),
            AudioUpdate::Unavailable { token: 0 }
        );

        // A local audio failure keeps the remote call stage alive and cannot
        // disable audio controls belonging to a later join.
        shared.active_audio_token.store(5, Ordering::SeqCst);
        shared.fail_audio(5, AudioFailure::MicrophoneUnavailable);
        assert_eq!(
            *shared.audio.borrow(),
            AudioUpdate::Failed {
                token: 0,
                reason: AudioFailure::MicrophoneUnavailable
            }
        );
        assert_eq!(shared.active_video_token.load(Ordering::SeqCst), 5);

        shared.active_audio_token.store(6, Ordering::SeqCst);
        shared.send_applied_audio(6, state);
        shared.fail_audio(5, AudioFailure::MicrophoneUnavailable);
        shared.clear_audio(5);
        shared.send_applied_audio(
            5,
            AudioState {
                microphone_muted: false,
                deafened: false,
            },
        );
        assert_eq!(
            *shared.audio.borrow(),
            AudioUpdate::Applied { token: 6, state }
        );

        // Choosing other devices after a failure restarts audio in the same
        // generation: re-arming shows it preparing again under its own
        // token, and never takes a later generation's audio.
        shared.active_audio_token.store(0, Ordering::SeqCst);
        shared.rearm_audio(5);
        assert_eq!(shared.active_audio_token.load(Ordering::SeqCst), 5);
        assert_eq!(
            *shared.audio.borrow(),
            AudioUpdate::Unavailable { token: 5 }
        );
        shared.active_audio_token.store(6, Ordering::SeqCst);
        shared.rearm_audio(5);
        assert_eq!(shared.active_audio_token.load(Ordering::SeqCst), 6);

        // Route reports render only for the current call generation, and
        // leaving clears them.
        let route = AudioRoute {
            generation: 1,
            microphone: DeviceRoute::Lost,
            speaker: DeviceRoute::Opening,
            notice: Some(RouteNotice::MicrophoneLost),
        };
        shared.send_audio_route(4, route.clone());
        shared.send_audio_route(0, route.clone());
        assert_eq!(*shared.audio_route.borrow(), AudioRouteUpdate::default());
        shared.send_audio_route(5, route.clone());
        assert_eq!(
            *shared.audio_route.borrow(),
            AudioRouteUpdate { token: 5, route }
        );
        shared.clear_media(5, 5);
        assert_eq!(*shared.audio_route.borrow(), AudioRouteUpdate::default());
    }

    #[test]
    fn the_call_page_names_each_route_side_and_claims_nothing_more() {
        let route = AudioRoute {
            generation: 3,
            microphone: DeviceRoute::Chosen {
                name: Some("Desk\u{202e} mic".into()),
                system_default: false,
            },
            speaker: DeviceRoute::DefaultInstead {
                name: Some("Built-in speakers".into()),
            },
            notice: Some(RouteNotice::SpeakerFellBack),
        };
        assert_eq!(
            route_summary(&route),
            "Microphone: Desk mic · Speaker: System default (Built-in speakers) instead"
        );
        let route = AudioRoute {
            microphone: DeviceRoute::Lost,
            speaker: DeviceRoute::Chosen {
                name: None,
                system_default: true,
            },
            ..route
        };
        assert_eq!(
            route_summary(&route),
            "Microphone: none in use · Speaker: System default"
        );
        let route = AudioRoute {
            microphone: DeviceRoute::Previous {
                name: Some("Desk mic".into()),
            },
            ..route
        };
        assert!(route_summary(&route).contains("Desk mic (the chosen one could not be opened)"));
        // The notices never claim a reroute that did not happen, and losing
        // the microphone never claims it was replaced.
        assert!(route_notice(RouteNotice::MicrophoneLost).contains("Nothing is sent"));
        assert!(
            route_notice(RouteNotice::MicrophoneRestored).contains("mute setting did not change")
        );
        assert!(route_notice(RouteNotice::SpeakerFellBack).contains("system default output"));
        for notice in [
            RouteNotice::Switched,
            RouteNotice::MicrophoneSwitchFailed,
            RouteNotice::SpeakerSwitchFailed,
            RouteNotice::MicrophoneLost,
            RouteNotice::MicrophoneDenied,
            RouteNotice::MicrophoneRestored,
            RouteNotice::SpeakerFellBack,
            RouteNotice::SpeakerLost,
            RouteNotice::SpeakerRestored,
            RouteNotice::DefaultSpeakerReopened,
            RouteNotice::ReconnectStopped,
            RouteNotice::MicrophoneNotResumed,
        ] {
            assert!(!route_notice(notice).contains("rejoin"), "{notice:?}");
        }
        // The status line calls audio ready only while both sides are on the
        // chosen devices (or still opening them).
        assert!(route_degraded(&route));
        let chosen = DeviceRoute::Chosen {
            name: None,
            system_default: true,
        };
        let healthy = AudioRoute {
            microphone: chosen.clone(),
            speaker: DeviceRoute::Opening,
            ..route.clone()
        };
        assert!(!route_degraded(&healthy));
        for side in [
            DeviceRoute::Lost,
            DeviceRoute::Previous { name: None },
            DeviceRoute::DefaultInstead { name: None },
        ] {
            assert!(route_degraded(&AudioRoute {
                speaker: side,
                ..healthy.clone()
            }));
        }
        for failure in [
            AudioFailure::MicrophoneUnavailable,
            AudioFailure::SpeakerUnavailable,
            AudioFailure::Other,
        ] {
            assert!(
                !failure.message().contains("leave and rejoin"),
                "{failure:?}"
            );
            assert!(
                failure.message().contains("Use for this call"),
                "{failure:?}"
            );
        }
    }

    #[test]
    fn queued_outgoing_request_suppresses_ring_before_call_token_or_lock_changes() {
        let gate = Arc::new(RingRequestGate::default());
        let initial = gate.epoch.load(Ordering::Acquire);
        assert!(gate.allowed(initial));
        let pending = gate.reserve().unwrap();
        let pending_epoch = gate.epoch.load(Ordering::Acquire);
        assert!(!gate.allowed(pending_epoch));
        assert!(gate.reserve().is_none());
        // Models cancellation or failure before account_transition/native join.
        drop(pending);
        assert!(!gate.allowed(initial));
        assert!(gate.allowed(pending_epoch));
    }

    struct TestCall {
        running: RunningCall,
        cancelled: oneshot::Receiver<()>,
        done: watch::Sender<Option<Result<(), String>>>,
        video: mpsc::Receiver<CaptureCommand>,
    }

    /// A call of the same account and room every time: only its generations
    /// tell two of them apart.
    fn test_call(token: u64, presentation: u64) -> TestCall {
        let (cancel, cancelled) = oneshot::channel();
        let (done, done_rx) = watch::channel(None);
        let (video_commands, video) = mpsc::channel(1);
        TestCall {
            running: RunningCall {
                token,
                presentation,
                account_id: "account-a".into(),
                room_id: "!room:example.org".into(),
                cancel: Some(cancel),
                done: done_rx,
                audio_controls: None,
                video_commands,
                capture_stop: Arc::new(CaptureStop::default()),
            },
            cancelled,
            done,
            video,
        }
    }

    fn test_joined() -> CallStatus {
        CallStatus::Joined {
            account_id: "account-a".into(),
            call: CallInfo {
                room_id: "!room:example.org".into(),
                slot_id: "m.call#room".into(),
                membership_id: "member".into(),
                local_identity: "identity".into(),
            },
        }
    }

    fn observed(call: u64, presentation: u64) -> ObservedCall {
        ObservedCall {
            account_id: "account-a".into(),
            room_id: "!room:example.org".into(),
            call,
            presentation,
        }
    }

    /// Install `running` joined, as `start_audio_owned` and a rejoin leave it:
    /// the call's own identity first, then its join's generations.
    fn install_test_call(shared: &Shared, slot: &mut CallSlot, running: RunningCall) {
        shared
            .active_call_token
            .store(running.token, Ordering::SeqCst);
        shared
            .active_video_token
            .store(running.presentation, Ordering::SeqCst);
        shared
            .active_audio_token
            .store(running.presentation, Ordering::SeqCst);
        *slot = CallSlot::Running(running);
        shared.status.send_replace(test_joined());
    }

    fn untouched(slot: &CallSlot, token: u64) -> bool {
        matches!(
            slot,
            CallSlot::Running(running)
                if running.token == token
                    && running.cancel.is_some()
                    && !running.capture_stop.requested()
        )
    }

    fn test_controller() -> (tempfile::TempDir, Arc<CallController>) {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let controller = Arc::new(CallController::new(manager, UpdateActivity::default()));
        (root, controller)
    }

    #[tokio::test]
    async fn observed_leave_ends_only_the_exact_call() {
        let (_root, controller) = test_controller();
        let TestCall {
            running,
            mut cancelled,
            done,
            mut video,
        } = test_call(9, 9);
        {
            let mut slot = controller.shared.slot.lock().await;
            install_test_call(&controller.shared, &mut slot, running);
        }
        // Call 7 was shown; call 9 replaced it in the same room and account.
        // Neither the names, a zero generation nor a refused admission match.
        let other_account = ObservedCall {
            account_id: "account-b".into(),
            ..observed(9, 9)
        };
        for (stale, admitted) in [
            (observed(7, 7), true),
            (observed(0, 0), true),
            (other_account, true),
            (observed(9, 9), false),
        ] {
            let admit = move || admitted;
            assert!(matches!(
                controller.end_call_for(&stale, &admit).await,
                Err(CallControllerError::StaleCall)
            ));
        }
        assert!(untouched(&*controller.shared.slot.lock().await, 9));
        assert!(matches!(
            cancelled.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(video.try_recv().is_err());
        assert_eq!(controller.status(), test_joined());
        assert_eq!(controller.video_token(), 9);
        assert_eq!(controller.call_token(), 9);

        let leaving = {
            let controller = controller.clone();
            tokio::spawn(async move { controller.end_call_for(&observed(9, 9), &|| true).await })
        };
        (&mut cancelled).await.unwrap();
        assert!(matches!(controller.status(), CallStatus::Ending { .. }));
        assert_eq!(controller.video_token(), 0);
        // Its own stable identity retires with it (compare-exchange on 9).
        assert_eq!(controller.call_token(), 0);
        // A second Leave of the same call joins nothing: it is already leaving.
        assert!(matches!(
            controller.end_call_for(&observed(9, 9), &|| true).await,
            Err(CallControllerError::StaleCall)
        ));
        done.send_replace(Some(Ok(())));
        leaving.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn queued_observed_actions_cannot_cross_a_same_room_replacement() {
        let (_root, controller) = test_controller();
        let first = test_call(7, 7);
        let mut held = controller.shared.slot.lock().await;
        install_test_call(&controller.shared, &mut held, first.running);
        // Clicks for call 7 queue behind the slot's lock (held as by its worker).
        let leave = {
            let controller = controller.clone();
            tokio::spawn(async move { controller.end_call_for(&observed(7, 7), &|| true).await })
        };
        let stop = {
            let controller = controller.clone();
            tokio::spawn(async move {
                controller
                    .stop_video_capture_for(&observed(7, 7), CaptureKind::Camera, &|| true)
                    .await
            })
        };
        let mute = {
            let controller = controller.clone();
            tokio::spawn(async move {
                controller
                    .set_microphone_muted_for(&observed(7, 7), true, &|| true)
                    .await
            })
        };
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert!(!leave.is_finished() && !stop.is_finished() && !mute.is_finished());
        // Call 7 ends and call 9 starts in the same room before they run.
        let TestCall {
            running,
            mut cancelled,
            done: _done,
            mut video,
        } = test_call(9, 9);
        install_test_call(&controller.shared, &mut held, running);
        drop(held);
        for action in [leave, stop, mute] {
            assert!(matches!(
                action.await.unwrap(),
                Err(CallControllerError::StaleCall)
            ));
        }
        assert!(untouched(&*controller.shared.slot.lock().await, 9));
        assert!(matches!(
            cancelled.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(video.try_recv().is_err());
        assert_eq!(controller.status(), test_joined());
        assert_eq!(controller.video_token(), 9);
        assert_eq!(controller.audio_token(), 9);
        assert_eq!(controller.call_token(), 9);
    }

    #[tokio::test]
    async fn a_full_capture_queue_refuses_an_observed_stop_without_waiting() {
        let (_root, controller) = test_controller();
        let TestCall {
            running,
            done: _done,
            mut video,
            ..
        } = test_call(7, 7);
        let worker_queue = running.video_commands.clone();
        {
            let mut slot = controller.shared.slot.lock().await;
            install_test_call(&controller.shared, &mut slot, running);
        }
        // The call's worker has not taken an earlier command yet.
        let (earlier, _earlier_reply) = oneshot::channel();
        worker_queue
            .try_send(CaptureCommand::Stop {
                kind: CaptureKind::Screen,
                reply: earlier,
            })
            .unwrap();
        assert!(matches!(
            controller
                .stop_video_capture_for(&observed(7, 7), CaptureKind::Camera, &|| true)
                .await,
            Err(CallControllerError::AlreadyBusy)
        ));
        // Only the earlier command is queued; nothing waited for space.
        assert!(matches!(
            video.try_recv(),
            Ok(CaptureCommand::Stop {
                kind: CaptureKind::Screen,
                ..
            })
        ));
        assert!(video.try_recv().is_err());
        assert!(untouched(&*controller.shared.slot.lock().await, 7));
    }

    #[tokio::test]
    async fn security_change_while_queued_refuses_the_observed_call() {
        let (_root, controller) = test_controller();
        let TestCall {
            running,
            mut cancelled,
            done: _done,
            mut video,
        } = test_call(7, 7);
        let mut held = controller.shared.slot.lock().await;
        install_test_call(&controller.shared, &mut held, running);
        // The click's own security context, checked again at admission.
        let context = Arc::new(AtomicBool::new(true));
        let leave = {
            let controller = controller.clone();
            let context = context.clone();
            tokio::spawn(async move {
                let admit = move || context.load(Ordering::SeqCst);
                controller.end_call_for(&observed(7, 7), &admit).await
            })
        };
        let stop = {
            let controller = controller.clone();
            let context = context.clone();
            tokio::spawn(async move {
                let admit = move || context.load(Ordering::SeqCst);
                controller
                    .stop_video_capture_for(&observed(7, 7), CaptureKind::Screen, &admit)
                    .await
            })
        };
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        context.store(false, Ordering::SeqCst);
        drop(held);
        for action in [leave, stop] {
            assert!(matches!(
                action.await.unwrap(),
                Err(CallControllerError::StaleCall)
            ));
        }
        assert!(untouched(&*controller.shared.slot.lock().await, 7));
        assert!(matches!(
            cancelled.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(video.try_recv().is_err());
        assert_eq!(controller.status(), test_joined());
    }

    #[tokio::test]
    async fn observed_media_controls_reach_only_the_exact_join() {
        let (_root, controller) = test_controller();
        let TestCall {
            running,
            mut cancelled,
            done,
            mut video,
        } = test_call(7, 7);
        {
            let mut slot = controller.shared.slot.lock().await;
            install_test_call(&controller.shared, &mut slot, running);
        }
        // The exact join: Stop reaches this call's own capture worker.
        let stopping = {
            let controller = controller.clone();
            tokio::spawn(async move {
                controller
                    .stop_video_capture_for(&observed(7, 7), CaptureKind::Camera, &|| true)
                    .await
            })
        };
        let Some(CaptureCommand::Stop { kind, reply }) = video.recv().await else {
            panic!("expected the observed join's Stop");
        };
        assert_eq!(kind, CaptureKind::Camera);
        reply.send(Ok(())).unwrap();
        stopping.await.unwrap().unwrap();
        // Mute reaches the join's audio (not running in this fixture).
        assert!(matches!(
            controller
                .set_microphone_muted_for(&observed(7, 7), true, &|| true)
                .await,
            Err(CallControllerError::Media(MediaError::ControlNotRunning))
        ));
        assert!(matches!(
            controller
                .set_deafened_for(&observed(7, 7), true, &|| false)
                .await,
            Err(CallControllerError::StaleCall)
        ));

        // A rejoin of the same call: the shown join's media is gone.
        {
            let mut slot = controller.shared.slot.lock().await;
            let CallSlot::Running(running) = &mut *slot else {
                unreachable!()
            };
            running.presentation = 8;
            controller
                .shared
                .active_video_token
                .store(8, Ordering::SeqCst);
            controller
                .shared
                .active_audio_token
                .store(8, Ordering::SeqCst);
        }
        assert_eq!(
            controller.observe(&controller.status()).await,
            Some(observed(7, 8))
        );
        assert!(matches!(
            controller
                .stop_video_capture_for(&observed(7, 7), CaptureKind::Camera, &|| true)
                .await,
            Err(CallControllerError::StaleCall)
        ));
        assert!(matches!(
            controller
                .set_microphone_muted_for(&observed(7, 7), false, &|| true)
                .await,
            Err(CallControllerError::StaleCall)
        ));
        assert!(matches!(
            controller
                .set_deafened_for(&observed(7, 8), true, &|| true)
                .await,
            Err(CallControllerError::Media(MediaError::ControlNotRunning))
        ));
        assert!(video.try_recv().is_err());
        // Leave still names the call through its rejoin.
        let leaving = {
            let controller = controller.clone();
            tokio::spawn(async move { controller.end_call_for(&observed(7, 7), &|| true).await })
        };
        (&mut cancelled).await.unwrap();
        // A leaving call's capture can no longer be addressed.
        assert!(matches!(
            controller
                .stop_video_capture_for(&observed(7, 8), CaptureKind::Screen, &|| true)
                .await,
            Err(CallControllerError::StaleCall)
        ));
        done.send_replace(Some(Ok(())));
        leaving.await.unwrap().unwrap();
        assert!(video.try_recv().is_err());
    }

    #[tokio::test]
    async fn observation_names_only_a_current_running_call() {
        let (_root, controller) = test_controller();
        assert_eq!(controller.observe(&controller.status()).await, None);
        let room = || "!room:example.org".to_owned();
        let account = || "account-a".to_owned();
        let TestCall {
            running,
            done: _done,
            ..
        } = test_call(7, 9);
        {
            let mut slot = controller.shared.slot.lock().await;
            install_test_call(&controller.shared, &mut slot, running);
        }
        // The call's own generation and its current join, read together.
        let joined = controller.status();
        assert_eq!(controller.observe(&joined).await, Some(observed(7, 9)));
        assert_eq!(
            controller.try_observe(&joined).unwrap(),
            Some(observed(7, 9))
        );
        // A status that is no longer current names nothing.
        let starting = CallStatus::Starting {
            account_id: account(),
            room_id: room(),
        };
        assert_eq!(controller.observe(&starting).await, None);
        controller.shared.status.send_replace(starting.clone());
        assert_eq!(controller.observe(&starting).await, Some(observed(7, 9)));
        assert_eq!(controller.observe(&joined).await, None);
        // A status naming another room never names the call in the slot.
        let elsewhere = CallStatus::Starting {
            account_id: account(),
            room_id: "!other:example.org".into(),
        };
        controller.shared.status.send_replace(elsewhere.clone());
        assert_eq!(controller.observe(&elsewhere).await, None);
        // While the slot is busy the UI thread cannot tell, and says so.
        controller.shared.status.send_replace(joined.clone());
        {
            let _held = controller.shared.slot.lock().await;
            assert!(controller.try_observe(&joined).is_err());
        }
        // A leave under way is no longer observable.
        {
            let mut slot = controller.shared.slot.lock().await;
            let CallSlot::Running(running) = &mut *slot else {
                unreachable!()
            };
            running.cancel.take();
        }
        assert_eq!(controller.observe(&joined).await, None);
        // Opening, leaving and a failed cleanup expose no exact generation.
        for status in [
            CallStatus::OpeningSlot {
                account_id: account(),
                room_id: room(),
            },
            CallStatus::Ending {
                account_id: account(),
                room_id: room(),
            },
            CallStatus::CleanupFailed {
                account_id: account(),
                room_id: room(),
                reason: "leave status unknown".into(),
            },
            CallStatus::Idle,
        ] {
            controller.shared.status.send_replace(status.clone());
            assert_eq!(controller.observe(&status).await, None);
        }
    }

    #[test]
    fn a_late_route_reply_claims_a_rejoin_only_for_the_same_call() {
        // Clicked on generation 9 of call 7.
        assert_eq!(route_reply_scope(9, 7, 9, 7), RouteReplyScope::Current);
        // Call 7 rejoined as generation 11.
        assert_eq!(route_reply_scope(9, 7, 11, 7), RouteReplyScope::Rejoined);
        // Call 7 ended, and another call (12) started: not a rejoin, whatever
        // its account or room.
        assert_eq!(route_reply_scope(9, 7, 12, 12), RouteReplyScope::Ended);
        assert_eq!(route_reply_scope(9, 7, 0, 0), RouteReplyScope::Ended);
        // Nothing to compare against: never claimed as this call.
        assert_eq!(route_reply_scope(9, 0, 9, 0), RouteReplyScope::Ended);
    }

    #[tokio::test]
    async fn a_route_chosen_before_the_worker_subscribes_is_still_applied() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let controller = CallController::new(manager, UpdateActivity::default());
        let shared = &controller.shared;
        let usb = AudioDeviceSelection {
            microphone: Some("coreaudio:usb".into()),
            speaker: None,
        };
        // Joined is published, a route is accepted (an unchanged repeat), then
        // the worker subscribes and starts the join (its own steps).
        shared.route_requests.send_replace(RouteRequest {
            token: 9,
            selection: usb.clone(),
            retry: true,
        });
        let mut requests = shared.route_requests.subscribe();
        assert!(
            !requests.has_changed().unwrap(),
            "already seen when subscribed"
        );
        let mut chosen = AudioDeviceSelection::default();
        let retry = begin_session_route(shared, &mut requests, 9, &mut chosen).await;
        assert!(retry, "a repeat admitted before the join started is kept");
        assert_eq!(chosen, usb);
        // The route the join opens is the baseline later requests compare with.
        assert_eq!(*shared.route_baseline.lock().unwrap(), (9, usb.clone()));
        // Another generation's queued choice is never taken.
        shared.route_requests.send_replace(RouteRequest {
            token: 8,
            ..RouteRequest::default()
        });
        let retry = begin_session_route(shared, &mut requests, 10, &mut chosen).await;
        assert!(!retry);
        assert_eq!(chosen, usb);
        assert_eq!(shared.route_baseline.lock().unwrap().0, 10);
    }

    #[tokio::test]
    async fn a_held_call_slot_bounds_a_route_switch_and_never_queues_it_late() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let controller = CallController::new(manager, UpdateActivity::default());
        let shared = Arc::clone(&controller.shared);
        shared.active_video_token.store(9, Ordering::SeqCst);
        shared.status.send_replace(CallStatus::Joined {
            account_id: "account-a".into(),
            call: CallInfo {
                room_id: "!room:example.org".into(),
                slot_id: "m.call#room".into(),
                membership_id: "member".into(),
                local_identity: "identity".into(),
            },
        });
        let (cancel, _cancelled) = oneshot::channel();
        let (_done, done) = watch::channel(None);
        let (video_commands, _video_rx) = mpsc::channel(1);
        let mut requests = shared.route_requests.subscribe();
        let usb = AudioDeviceSelection {
            microphone: Some("coreaudio:usb".into()),
            speaker: None,
        };
        let limit = Duration::from_millis(50);
        // A worker wedged while holding the slot of a running call that would
        // otherwise admit this request: the switch gives up, and queues
        // nothing later once the slot is free.
        let mut held = shared.slot.lock().await;
        *held = CallSlot::Running(RunningCall {
            token: 7,
            presentation: 9,
            account_id: "account-a".into(),
            room_id: "!room:example.org".into(),
            cancel: Some(cancel),
            done,
            audio_controls: None,
            video_commands,
            capture_stop: Arc::new(CaptureStop::default()),
        });
        let admission = admit_route_request(&shared, 9, "account-a", usb.clone());
        let result =
            bounded_route_admission(&shared, controller.subscribe_status(), 9, limit, admission)
                .await;
        assert!(matches!(result, Err(CallControllerError::TimedOut(_))));
        drop(held);
        tokio::task::yield_now().await;
        assert!(
            !requests.has_changed().unwrap(),
            "a timed-out admission never queues"
        );

        // Positive control with the slot free: each admission queues once, and
        // only an unchanged repeat of the route the generation runs with is a
        // retry (the first one included, with no click before it), so a
        // change and its undo never look like one.
        let speaker = AudioDeviceSelection {
            speaker: Some("coreaudio:desk".into()),
            ..usb.clone()
        };
        let other = AudioDeviceSelection::default();
        assert_eq!(*shared.route_baseline.lock().unwrap(), (0, other.clone()));
        let mut chosen = usb.clone();
        let mut started = shared.route_requests.subscribe();
        assert!(!begin_session_route(&shared, &mut started, 9, &mut chosen).await);
        for (selection, retry) in [
            (usb.clone(), true),
            (usb.clone(), true),
            (speaker, false),
            (usb.clone(), false),
            (other, false),
        ] {
            let admission = admit_route_request(&shared, 9, "account-a", selection.clone());
            bounded_route_admission(&shared, controller.subscribe_status(), 9, limit, admission)
                .await
                .unwrap();
            let queued = requests.borrow_and_update().clone();
            assert_eq!(
                (queued.token, queued.selection, queued.retry),
                (9, selection, retry)
            );
        }

        // The call ending while it waits abandons it at once, not by the bound.
        let held = shared.slot.lock().await;
        let admission = admit_route_request(&shared, 9, "account-a", usb);
        let pending = bounded_route_admission(
            &shared,
            controller.subscribe_status(),
            9,
            Duration::from_secs(2),
            admission,
        );
        let end = async {
            shared.active_video_token.store(0, Ordering::SeqCst);
            shared.status.send_replace(CallStatus::Idle);
        };
        let started = tokio::time::Instant::now();
        let (result, ()) = tokio::join!(pending, end);
        assert!(matches!(result, Err(CallControllerError::StaleCall)));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "abandoned at once"
        );
        drop(held);
        tokio::task::yield_now().await;
        assert!(!requests.has_changed().unwrap(), "nothing was queued");
    }

    #[test]
    fn a_rejoin_after_a_join_lost_before_its_first_report_keeps_an_unknown_microphone_closed() {
        // The worker's session-to-rejoin hand-off. A first join lost before
        // its audio recorded anything (the open or the first publication
        // still pending) has no point of its own and carried none.
        let default = AudioDeviceSelection::default();
        let alias = AudioDeviceSelection {
            microphone: Some("alsa:default".into()),
            speaker: None,
        };
        let usb = AudioDeviceSelection {
            microphone: Some("coreaudio:usb".into()),
            speaker: None,
        };
        // System default or an alias may name another microphone now: closed.
        for chosen in [&default, &alias] {
            let point = rejoin_resume_point(None, None, chosen);
            assert_eq!(point.retained_microphone(), None, "{chosen:?}");
        }
        // An exact chosen device is the only one reopened.
        assert_eq!(
            rejoin_resume_point(None, None, &usb).retained_microphone(),
            Some("coreaudio:usb".into())
        );
        // The lost join's own point (recorded before its first publication)
        // wins over the one it carried and over the route.
        let recorded = AudioResume::unestablished(&usb);
        let carried = AudioResume::unestablished(&default);
        let point = rejoin_resume_point(Some(recorded), Some(carried.clone()), &default);
        assert_eq!(point.retained_microphone(), Some("coreaudio:usb".into()));
        assert_eq!(
            rejoin_resume_point(None, Some(carried.clone()), &usb),
            carried
        );
    }

    #[tokio::test]
    async fn a_retry_cut_off_by_a_rejoin_is_not_carried_and_the_reply_says_so() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let controller = CallController::new(manager, UpdateActivity::default());
        let shared = Arc::clone(&controller.shared);
        shared.active_video_token.store(9, Ordering::SeqCst);
        shared.status.send_replace(CallStatus::Joined {
            account_id: "account-a".into(),
            call: CallInfo {
                room_id: "!room:example.org".into(),
                slot_id: "m.call#room".into(),
                membership_id: "member".into(),
                local_identity: "identity".into(),
            },
        });
        let (cancel, _cancelled) = oneshot::channel();
        let (_done, done) = watch::channel(None);
        let (video_commands, _video_rx) = mpsc::channel(1);
        *shared.slot.lock().await = CallSlot::Running(RunningCall {
            token: 7,
            presentation: 9,
            account_id: "account-a".into(),
            room_id: "!room:example.org".into(),
            cancel: Some(cancel),
            done,
            audio_controls: None,
            video_commands,
            capture_stop: Arc::new(CaptureStop::default()),
        });
        // System default, with the microphone left unresumed (no exact ID).
        let default = AudioDeviceSelection::default();
        let mut worker = shared.route_requests.subscribe();
        let limit = Duration::from_secs(2);
        for consumed in [false, true] {
            // Generation 9's join starts on System default (the baseline).
            let mut chosen = default.clone();
            assert!(!begin_session_route(&shared, &mut worker, 9, &mut chosen).await);
            // An unchanged Use for this call: admitted as a retry.
            let admission = admit_route_request(&shared, 9, "account-a", default.clone());
            bounded_route_admission(&shared, controller.subscribe_status(), 9, limit, admission)
                .await
                .unwrap();
            assert!(shared.route_requests.borrow().retry);
            if consumed {
                // The lost join's worker had already taken it.
                let _ = worker.borrow_and_update();
            }
            // The join is lost (a higher-priority branch) and rejoins as 11.
            {
                let _slot = shared.slot.lock().await;
                shared.retag_route_for_rejoin(9, 11);
            }
            let mut chosen = default.clone();
            let retry = begin_session_route(&shared, &mut worker, 11, &mut chosen).await;
            assert!(!retry, "consumed={consumed}: a retry is never carried");
            assert_eq!(chosen, default);
            // That rejoin keeps the unknown microphone closed.
            let point = rejoin_resume_point(None, None, &chosen);
            assert_eq!(point.retained_microphone(), None);
            // The delayed reply for call 7 sees generation 11: no promise.
            let scope = route_reply_scope(9, 7, 11, 7);
            assert_eq!(scope, RouteReplyScope::Rejoined);
            let notice = route_reply_notice(scope, true, "Switch requested.");
            assert!(!notice.contains("switches to these devices"), "{notice}");
            assert!(
                notice.contains("choose Use for this call again"),
                "{notice}"
            );
        }
        assert_eq!(
            route_reply_notice(RouteReplyScope::Current, true, "Switch requested."),
            "Switch requested."
        );
    }
}
