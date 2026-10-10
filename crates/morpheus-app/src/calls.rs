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
//! Live microphone mute and local deafen are bound to the current call join.
//! Local camera publishing is not exposed here yet.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use morpheus_matrix::{AccountManager, MatrixCoreError, SyncBackend, SyncStatus};
pub use morpheus_rtc::CallInfo;
use morpheus_rtc::{
    AudioControls, AudioState, CallOptions, LocalRtcSupport, MediaError, RtcError, RtcService,
    TransportSupport, local_device_support, local_support,
};
use tokio::{
    sync::{Mutex, oneshot, watch},
    time::timeout,
};

use crate::video_render::{RosterUpdate, VideoUpdate, run_remote_video};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const JOIN_TIMEOUT: Duration = Duration::from_secs(45);
const LEAVE_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(75);

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

/// Choose the MatrixRTC membership format before joining. The compatibility
/// mode is only for Element Call's legacy room-state format; it does not
/// advertise support for current MatrixRTC/Matrix 2.0 calls.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CallJoinMode {
    #[default]
    Standard,
    ElementCallRoomState,
}

impl CallJoinMode {
    fn options(self) -> CallOptions {
        match self {
            Self::Standard => CallOptions::default(),
            Self::ElementCallRoomState => CallOptions::room_state_compatibility(),
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
}

impl CallStatus {
    /// The UI should wait for `end_call()` before switching or logging out.
    pub fn blocks_account_switch(&self) -> bool {
        matches!(
            self,
            Self::OpeningSlot { .. }
                | Self::Starting { .. }
                | Self::Joined { .. }
                | Self::Ending { .. }
                | Self::CleanupFailed { .. }
        )
    }
}

#[derive(Debug)]
pub enum CallControllerError {
    Unavailable(AudioCallCapability),
    AlreadyBusy,
    Cancelled,
    AccountChanged,
    StaleCall,
    RoomNotLoaded,
    UnknownEncryptionState,
    RoomState(String),
    Matrix(MatrixCoreError),
    Rtc(RtcError),
    Media(MediaError),
    WorkerStopped,
    TimedOut(&'static str),
    Cleanup(String),
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
    account_id: String,
    room_id: String,
    cancel: Option<oneshot::Sender<()>>,
    done: watch::Receiver<Option<Result<(), String>>>,
    audio_controls: Option<AudioControls>,
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
    },
    Applied {
        token: u64,
        state: AudioState,
    },
}

impl AudioUpdate {
    pub fn token(self) -> u64 {
        match self {
            Self::Unavailable { token } | Self::Failed { token } | Self::Applied { token, .. } => {
                token
            }
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

struct Shared {
    slot: Mutex<CallSlot>,
    status: watch::Sender<CallStatus>,
    video: watch::Sender<VideoUpdate>,
    roster: watch::Sender<RosterUpdate>,
    audio: watch::Sender<AudioUpdate>,
    active_video_token: AtomicU64,
    active_audio_token: AtomicU64,
    next_token: AtomicU64,
}

impl Shared {
    async fn reserve_open_slot(
        &self,
        account_id: &str,
        room_id: &str,
    ) -> Result<(u64, watch::Sender<bool>), CallControllerError> {
        let mut slot = self.slot.lock().await;
        if !matches!(*slot, CallSlot::Idle) {
            return Err(CallControllerError::AlreadyBusy);
        }
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
            self.status.send_replace(CallStatus::Idle);
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

    fn fail_audio(&self, token: u64) {
        if self
            .active_audio_token
            .compare_exchange(token, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.audio.send_replace(AudioUpdate::Failed { token: 0 });
        }
    }

    fn clear_media(&self, token: u64) {
        self.clear_video(token);
        self.clear_audio(token);
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
    pub fn new(manager: Arc<AccountManager>) -> Self {
        let (status, _) = watch::channel(CallStatus::Idle);
        let (video, _) = watch::channel(VideoUpdate::Clear { token: 0 });
        let (roster, _) = watch::channel(RosterUpdate {
            token: 0,
            members: Vec::new(),
        });
        let (audio, _) = watch::channel(AudioUpdate::Unavailable { token: 0 });
        Self {
            manager,
            shared: Arc::new(Shared {
                slot: Mutex::new(CallSlot::Idle),
                status,
                video,
                roster,
                audio,
                active_video_token: AtomicU64::new(0),
                active_audio_token: AtomicU64::new(0),
                next_token: AtomicU64::new(1),
            }),
        }
    }

    pub fn subscribe_status(&self) -> watch::Receiver<CallStatus> {
        self.shared.status.subscribe()
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
        if running.token != token || running.cancel.is_none() {
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
        expected_account_id: &str,
        room_id: &str,
        mode: CallJoinMode,
    ) -> Result<CallInfo, CallControllerError> {
        let account =
            self.manager
                .active_account()
                .await
                .ok_or(CallControllerError::Unavailable(
                    AudioCallCapability::SignInRequired,
                ))?;
        if account.id != expected_account_id {
            return Err(CallControllerError::AccountChanged);
        }
        let token = self.shared.next_token.fetch_add(1, Ordering::Relaxed);
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let (done_tx, done_rx) = watch::channel(None);
        let (ready_tx, ready_rx) = oneshot::channel();
        {
            let mut slot = self.shared.slot.lock().await;
            if !matches!(*slot, CallSlot::Idle) {
                return Err(CallControllerError::AlreadyBusy);
            }
            *slot = CallSlot::Running(RunningCall {
                token,
                account_id: account.id.clone(),
                room_id: room_id.to_owned(),
                cancel: Some(cancel_tx),
                done: done_rx,
                audio_controls: None,
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
            self.shared.status.send_replace(CallStatus::Starting {
                account_id: account.id.clone(),
                room_id: room_id.to_owned(),
            });
        }
        let worker = CallWorker {
            manager: Arc::clone(&self.manager),
            shared: Arc::clone(&self.shared),
            account_id: account.id,
            room_id: room_id.to_owned(),
            options: mode.options(),
            token,
            cancel: cancel_rx,
            done: done_tx,
            ready: Some(ready_tx),
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
                    self.shared.status.send_replace(CallStatus::Idle);
                }
                return Ok(());
            }
            None => {}
        }
        let (cancel, mut done, token, account_id, room_id) = {
            let mut slot = self.shared.slot.lock().await;
            let running = match &mut *slot {
                CallSlot::Idle => return Ok(()),
                CallSlot::OpeningSlot { .. } | CallSlot::CleanupFailed { .. } => {
                    return Err(CallControllerError::AlreadyBusy);
                }
                CallSlot::Running(running) => running,
            };
            let cancel = running.cancel.take();
            let done = running.done.clone();
            self.shared.status.send_replace(CallStatus::Ending {
                account_id: running.account_id.clone(),
                room_id: running.room_id.clone(),
            });
            (
                cancel,
                done,
                running.token,
                running.account_id.clone(),
                running.room_id.clone(),
            )
        };
        if let Some(cancel) = cancel {
            let _ = cancel.send(());
        }
        self.shared.clear_media(token);
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
        if self.shared.active_audio_token.load(Ordering::SeqCst) == token
            || self.shared.active_video_token.load(Ordering::SeqCst) == token
        {
            return Err(CallControllerError::AlreadyBusy);
        }
        let mut slot = self.shared.slot.lock().await;
        if matches!(&*slot, CallSlot::CleanupFailed { token: current, .. } if *current == token) {
            *slot = CallSlot::Idle;
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

struct CallWorker {
    manager: Arc<AccountManager>,
    shared: Arc<Shared>,
    account_id: String,
    room_id: String,
    options: CallOptions,
    token: u64,
    cancel: oneshot::Receiver<()>,
    done: watch::Sender<Option<Result<(), String>>>,
    ready: Option<oneshot::Sender<Result<CallInfo, CallControllerError>>>,
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
        ) || ensure_account(&self.manager, &self.account_id)
            .await
            .is_err()
        {
            self.shared.clear_media(self.token);
            let leave = leave_with_timeout(&mut rtc, &self.room_id).await;
            self.send_ready(Err(CallControllerError::Cancelled));
            self.finish(leave.map_err(|error| error.to_string()), None)
                .await;
            return;
        }
        let controls = match rtc.audio_controls(&self.room_id) {
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
                    if running.token == self.token && running.cancel.is_some() =>
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
        if self.send_ready(Ok(info)) {
            let leave = leave_with_timeout(&mut rtc, &self.room_id).await;
            self.finish(leave.map_err(|error| error.to_string()), None)
                .await;
            return;
        }

        let mut active = self.manager.subscribe_active_account();
        let mut sync = self.manager.subscribe_sync_status();
        let account_still_active = active.borrow().as_ref().map(|account| account.id.as_str())
            == Some(self.account_id.as_str());
        let sync_state = sync.borrow().clone();
        if !account_still_active
            || !matches!(
                sync_state,
                SyncStatus::Running {
                    backend: SyncBackend::SlidingSticky
                }
            )
        {
            self.shared.clear_media(self.token);
            let reason = if account_still_active {
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
        let media_result = {
            let audio = rtc.run_audio_controlled(&self.room_id, &controls);
            let mut applied_audio = controls.subscribe_applied_state();
            let video = run_remote_video(
                &rtc,
                &self.room_id,
                self.token,
                self.shared.video.clone(),
                self.shared.roster.clone(),
                &self.shared.active_video_token,
            );
            tokio::pin!(audio);
            tokio::pin!(video);
            let mut audio_running = true;
            let mut video_running = true;
            loop {
                tokio::select! {
                    _ = &mut self.cancel => break Ok(()),
                    changed = active.changed() => {
                        if changed.is_err()
                            || active.borrow().as_ref().map(|account| account.id.as_str()) != Some(self.account_id.as_str()) {
                            break Err(CallControllerError::AccountChanged);
                        }
                    }
                    changed = sync.changed() => {
                        let state = sync.borrow().clone();
                        if changed.is_err() || !matches!(state, SyncStatus::Running { backend: SyncBackend::SlidingSticky }) {
                            break Err(CallControllerError::Unavailable(AudioCallCapability::StickySyncRequired(state)));
                        }
                    }
                    result = &mut audio, if audio_running => match result {
                        Ok(()) => break Ok(()),
                        Err(MediaError::CallEnded) => break Err(CallControllerError::Media(MediaError::CallEnded)),
                        Err(_) => {
                            // A missing or stuck local audio device must not
                            // tear down the receive-only call stage.
                            audio_running = false;
                            self.shared.fail_audio(self.token);
                        }
                    },
                    changed = applied_audio.changed() => {
                        if changed.is_ok() && controls.is_running() {
                            self.shared.send_applied_audio(self.token, *applied_audio.borrow_and_update());
                        }
                    }
                    // A closed roster/video stream must not end microphone
                    // audio. run_remote_video normally waits for cancellation.
                    () = &mut video, if video_running => video_running = false,
                }
            }
        };
        self.shared.clear_media(self.token);
        let leave_result = leave_with_timeout(&mut rtc, &self.room_id).await;
        let cleanup = leave_result.map_err(|error| error.to_string());
        self.finish(cleanup, media_result.err().map(|error| error.to_string()))
            .await;
    }

    async fn setup(&mut self) -> Result<(RtcService, CallInfo), CallControllerError> {
        let controller = CallController {
            manager: Arc::clone(&self.manager),
            shared: Arc::clone(&self.shared),
        };
        let probe = tokio::select! {
            biased;
            _ = &mut self.cancel => return Err(CallControllerError::Cancelled),
            result = controller.probe_audio() => result?,
        };
        match probe {
            AudioCallCapability::TransportReady { .. } => {}
            unavailable => return Err(CallControllerError::Unavailable(unavailable)),
        }
        ensure_account(&self.manager, &self.account_id).await?;
        let client = tokio::select! {
            biased;
            _ = &mut self.cancel => return Err(CallControllerError::Cancelled),
            result = self.manager.active_client() => result?,
        };
        ensure_account(&self.manager, &self.account_id).await?;
        let mut rtc = RtcService::new(client);
        if !matches!(
            self.cancel.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ) {
            return Err(CallControllerError::Cancelled);
        }
        let room_id = self.room_id.clone();
        let mut cancelled = false;
        // Once joining starts, keep the upstream join future alive until it
        // yields a call handle. It can publish membership before connecting
        // media; dropping it on cancellation would skip an explicit leave.
        let joined = {
            let join = timeout(JOIN_TIMEOUT, rtc.join(&room_id, self.options.clone()));
            tokio::pin!(join);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut self.cancel, if !cancelled => {
                        cancelled = true;
                        if let Some(ready) = self.ready.take() {
                            let _ = ready.send(Err(CallControllerError::Cancelled));
                        }
                    }
                    result = &mut join => break result,
                }
            }
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
        if let Err(error) = ensure_account(&self.manager, &self.account_id).await {
            leave_with_timeout(&mut rtc, &room_id)
                .await
                .map_err(|cleanup| {
                    CallControllerError::Cleanup(format!(
                        "{error}; cleanup after account change also failed: {cleanup}"
                    ))
                })?;
            return Err(error);
        }
        Ok((rtc, info))
    }

    /// Return true if the caller waiting for startup has gone away.
    fn send_ready(&mut self, result: Result<CallInfo, CallControllerError>) -> bool {
        self.ready
            .take()
            .is_none_or(|sender| sender.send(result).is_err())
    }

    async fn finish(&self, cleanup: Result<(), String>, failure: Option<String>) {
        self.shared.clear_media(self.token);
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
                    self.shared.status.send_replace(match failure {
                        None => CallStatus::Idle,
                        Some(ref reason) => CallStatus::Failed {
                            room_id: self.room_id.clone(),
                            reason: reason.clone(),
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

    #[tokio::test]
    async fn opening_slot_blocks_account_switch_until_write_finishes() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let controller = Arc::new(CallController::new(manager));
        let (token, done) = controller
            .shared
            .reserve_open_slot("account-a", "!room-a:example.org")
            .await
            .unwrap();
        assert!(controller.status().blocks_account_switch());
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
    }

    #[tokio::test]
    async fn failed_leave_requires_distinct_acknowledgement_after_worker_terminates() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let controller = Arc::new(CallController::new(manager));
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
        controller.abandon_failed_leave().await.unwrap();
        assert!(!controller.status().blocks_account_switch());
    }

    #[tokio::test]
    async fn acknowledgement_waits_for_worker_completion() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(AccountManager::open(root.path()).unwrap());
        let controller = Arc::new(CallController::new(manager));
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

    #[test]
    fn call_modes_keep_current_slot_and_legacy_room_state_separate() {
        use morpheus_rtc::InteropMode;

        let standard = CallJoinMode::Standard.options();
        assert_eq!(standard.interop_mode, InteropMode::Current);
        assert_eq!(standard.slot_id, "m.call#room");

        let legacy = CallJoinMode::ElementCallRoomState.options();
        assert_eq!(legacy.interop_mode, InteropMode::RoomState);
        assert_eq!(legacy.slot_id, "m.call#room");
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
        let shared = Shared {
            slot: Mutex::new(CallSlot::Idle),
            status,
            video,
            roster,
            audio,
            active_video_token: AtomicU64::new(5),
            active_audio_token: AtomicU64::new(5),
            next_token: AtomicU64::new(6),
        };
        shared.clear_video(5);
        assert_eq!(shared.active_video_token.load(Ordering::SeqCst), 0);
        shared.video.send_replace(VideoUpdate::Clear { token: 5 });
        shared.roster.send_replace(RosterUpdate {
            token: 5,
            members: Vec::new(),
        });
        shared.clear_video(5);
        assert_eq!(shared.video.borrow().token(), 0);
        assert_eq!(shared.roster.borrow().token, 0);

        shared.active_video_token.store(6, Ordering::SeqCst);
        shared.video.send_replace(VideoUpdate::Clear { token: 6 });
        shared.clear_video(5);
        assert_eq!(shared.video.borrow().token(), 6);
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
        let shared = Shared {
            slot: Mutex::new(CallSlot::Idle),
            status,
            video,
            roster,
            audio,
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
        shared.fail_audio(5);
        assert_eq!(*shared.audio.borrow(), AudioUpdate::Failed { token: 0 });
        assert_eq!(shared.active_video_token.load(Ordering::SeqCst), 5);

        shared.active_audio_token.store(6, Ordering::SeqCst);
        shared.send_applied_audio(6, state);
        shared.fail_audio(5);
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
}
