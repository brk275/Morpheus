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
    AudioControls, AudioDeviceSelection, AudioState, CallOptions, LocalRtcSupport, MediaError,
    ReconnectBackoff, ReconnectPolicy, ReconnectWindow, RtcCallEnd, RtcConnectionSignal, RtcError,
    RtcService, TransportSupport, local_device_support, local_support,
};
use tokio::{
    sync::{Mutex, broadcast, mpsc, oneshot, watch},
    time::timeout,
};

use crate::{
    call_recovery::{
        RejoinCause, SIGNALING_GRACE, SignalingHealth, rejoin_retryable, signaling_health,
    },
    incoming_ring::controller::{AcceptedOffer, EstablishedAuthorization, EstablishedGuard},
    local_video::{
        CaptureCommand, CaptureKind, CaptureSource, CaptureStop, LocalVideoSession,
        LocalVideoStatus,
    },
    outgoing_ring::{InvitationOutcome, OutgoingRing},
    updater::{CallLease as UpdateCallLease, UpdateActivity, UpdateError},
    video_render::{RosterUpdate, VideoUpdate, run_remote_video},
    voice_spaces::VoiceSpaces,
};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const JOIN_TIMEOUT: Duration = Duration::from_secs(45);
const LEAVE_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(75);
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
                "Microphone unavailable or ambiguous. Choose a microphone in Settings, then leave and rejoin."
            }
            Self::SpeakerUnavailable => {
                "Speaker unavailable or ambiguous. Choose a speaker in Settings, then leave and rejoin."
            }
            Self::Other => {
                "Local audio could not start or stopped. Check device permissions and Settings, then leave and rejoin."
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
    audio: watch::Sender<AudioUpdate>,
    local_video: watch::Sender<LocalVideoStatus>,
    active_capture_workers: Arc<AtomicUsize>,
    active_video_token: AtomicU64,
    active_audio_token: AtomicU64,
    next_token: AtomicU64,
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

    async fn finish_open_slot(&self, token: u64) {
        let mut slot = self.slot.lock().await;
        if matches!(&*slot, CallSlot::OpeningSlot { token: current, .. } if *current == token) {
            *slot = CallSlot::Idle;
            self.release_update_lease();
            self.status.send_replace(CallStatus::Idle);
        }
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

    fn clear_media(&self, token: u64) {
        self.clear_video(token);
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

    fn clear_video(&self, token: u64) {
        let cleared = self
            .active_video_token
            .compare_exchange(token, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
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
                audio,
                local_video,
                active_capture_workers: Arc::new(AtomicUsize::new(0)),
                active_video_token: AtomicU64::new(0),
                active_audio_token: AtomicU64::new(0),
                next_token: AtomicU64::new(1),
            }),
        }
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

    pub fn subscribe_audio(&self) -> watch::Receiver<AudioUpdate> {
        self.shared.audio.subscribe()
    }

    pub fn subscribe_local_video(&self) -> watch::Receiver<LocalVideoStatus> {
        self.shared.local_video.subscribe()
    }

    /// A queued UI update may only render for the current call generation.
    pub fn video_token(&self) -> u64 {
        self.shared.active_video_token.load(Ordering::SeqCst)
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
            shared.finish_open_slot(token).await;
            done.send_replace(true);
            let _ = result_tx.send(result);
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
        enum PriorOperation {
            Opening {
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
                CallSlot::OpeningSlot {
                    account_id,
                    room_id,
                    done,
                    ..
                } => {
                    self.shared.status.send_replace(CallStatus::Ending {
                        account_id: account_id.clone(),
                        room_id: room_id.clone(),
                    });
                    Some(PriorOperation::Opening { done: done.clone() })
                }
                CallSlot::CleanupFailed { token, done } => Some(PriorOperation::CleanupFailed {
                    token: *token,
                    done: done.clone(),
                }),
                _ => None,
            }
        };
        match prior {
            Some(PriorOperation::Opening { mut done }) => {
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
        self.shared.clear_media(presentation);
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
        let (token, mut done) = {
            let slot = self.shared.slot.lock().await;
            match &*slot {
                CallSlot::Idle => return Ok(()),
                CallSlot::CleanupFailed { token, done } => (*token, done.clone()),
                _ => return Err(CallControllerError::AlreadyBusy),
            }
        };
        let _ = wait_cleanup_done(&mut done).await?;
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

// Read by the detached audio opener as well as the async call owner. Account
// and sync invalidation must not wait for the owner's next select-loop poll.
// A transient sync loss pauses transmission in the worker instead; only the
// end of sticky sync for this session revokes capture.
fn audio_capture_allowed(stop: &CaptureStop, selection_current: bool, sync: &SyncStatus) -> bool {
    !stop.requested()
        && selection_current
        && signaling_health(sync) != SignalingHealth::Unavailable
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
        let setup = self.setup().await;
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
            self.shared.clear_media(self.presentation);
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
            self.shared.clear_media(self.presentation);
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
        let mut invitation_pending = self.intent == CallStartIntent::Notify;
        let mut invitation_deadline = None;
        let mut invitation_receipt = None;
        let mut recoveries = ReconnectWindow::per_call();
        if invitation_pending {
            self.shared
                .send_invitation(self.presentation, &self.selection, "Preparing invitation…");
        }
        // One iteration per join. A lost join is left and replaced in place:
        // the call generation, its running captures and its Leave stay valid.
        let (outcome, rejoin_cleanup) = loop {
            // A platform publish operation can suspend the call worker. These
            // watchers stop OS capture immediately even if the worker has not
            // yet returned to its main account/sync select loop.
            let guards = self.spawn_capture_guards(
                &active,
                &sync,
                incoming_guard.as_ref(),
                Some(&controls),
            );
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
                let audio = rtc.run_audio_controlled_with_devices(
                    &self.room_id,
                    &controls,
                    self.audio_devices.clone(),
                );
                let mut applied_audio = controls.subscribe_applied_state();
                let video = run_remote_video(
                    &rtc,
                    &self.room_id,
                    self.presentation,
                    self.shared.video.clone(),
                    self.shared.roster.clone(),
                    &self.shared.active_video_token,
                );
                tokio::pin!(audio);
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
                            Ok(event) => match RtcConnectionSignal::from_event(&event) {
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
                                self.shared.fail_audio(self.presentation, AudioFailure::from_error(&error));
                            }
                        },
                        changed = applied_audio.changed() => {
                            if changed.is_ok() && controls.is_running() {
                                self.shared.send_applied_audio(self.presentation, *applied_audio.borrow_and_update());
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
            if invitation_pending {
                // The invitation named the lost membership. It is never resent.
                invitation_pending = false;
                self.shared
                    .send_invitation(self.presentation, &self.selection, INVITATION_INTERRUPTED);
            }
            outgoing.detach_publications();
            if !self.advance_presentation(&mut outgoing).await {
                // Leave or an account change won before the rejoin began.
                break match self.stop_result() {
                    Ok(()) => (CallOutcome::Idle, Ok(())),
                    Err(error) => (CallOutcome::Failed(error.to_string()), Ok(())),
                };
            }
            let requested = controls.requested_state();
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
        self.shared.clear_media(self.presentation);
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
        let sync = sync.clone();
        let incoming = incoming_guard.cloned();
        let info = info.clone();
        rtc.audio_controls_cancellable(&self.room_id, move || {
            audio_capture_allowed(&stop, selection.is_current(), &sync.borrow())
                && incoming_capture_allowed(incoming.as_ref(), &info)
        })
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
    ) -> Result<CallInfo, JoinAttemptError> {
        let room_id = self.room_id.clone();
        let (joined, cancelled) = {
            let join = timeout(JOIN_TIMEOUT, rtc.join(&room_id, self.options.clone()));
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
            match self.join_attempt(rtc, inputs.outgoing).await {
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
        self.shared.clear_media(self.presentation);
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
        controller.shared.finish_open_slot(token).await;
        done.send_replace(true);
        waiting.await.unwrap().unwrap();
        assert!(!controller.status().blocks_account_switch());
        assert!(controller.shared.update_activity.try_install().is_ok());
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
        controller.shared.finish_open_slot(token + 1).await;
        assert!(matches!(
            activity.try_install(),
            Err(UpdateError::CallActive)
        ));
        controller.shared.finish_open_slot(token).await;
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
            audio,
            local_video,
            active_capture_workers: Arc::new(AtomicUsize::new(0)),
            active_video_token: AtomicU64::new(5),
            active_audio_token: AtomicU64::new(5),
            next_token: AtomicU64::new(6),
        };
        shared.invitation.send_replace(InvitationUpdate {
            token: 5,
            text: "Invitation sent",
        });
        shared.clear_media(5);
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
        shared.clear_media(5);
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
        shared.clear_media(5);
        assert_eq!(*shared.invitation.borrow(), current_invitation);
        assert_eq!(shared.video.borrow().token(), 6);
        assert_eq!(shared.local_video.borrow().token, 6);
        assert_eq!(shared.local_video.borrow().screen.source, "New display");
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
            audio,
            local_video,
            active_capture_workers: Arc::new(AtomicUsize::new(0)),
            active_video_token: AtomicU64::new(5),
            active_audio_token: AtomicU64::new(5),
            next_token: AtomicU64::new(6),
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
}
