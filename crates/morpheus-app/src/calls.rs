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
//! A failed leave keeps the switch blocked until the user explicitly confirms
//! the action again. Upstream consumes the call handle during leave, so the
//! second confirmation acknowledges the error; it cannot retry that handle.
//!
//! `open_slot()` writes `m.rtc.slot` only on an explicit user action. It reads
//! the room's current encryption state and never uses Morpheus's optional
//! voice-space presentation marker to decide permissions or call semantics.
//! Live mute, deafen, camera, and video rendering are not exposed here yet.

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
    CallOptions, LocalRtcSupport, MediaError, RtcError, RtcService, TransportSupport,
    local_device_support, local_support,
};
use tokio::{
    sync::{Mutex, oneshot, watch},
    time::timeout,
};

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallStatus {
    Idle,
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
            Self::Starting { .. }
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
}

enum CallSlot {
    Idle,
    Running(RunningCall),
    CleanupFailed { token: u64 },
}

struct Shared {
    slot: Mutex<CallSlot>,
    status: watch::Sender<CallStatus>,
    next_token: AtomicU64,
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
        Self {
            manager,
            shared: Arc::new(Shared {
                slot: Mutex::new(CallSlot::Idle),
                status,
                next_token: AtomicU64::new(1),
            }),
        }
    }

    pub fn subscribe_status(&self) -> watch::Receiver<CallStatus> {
        self.shared.status.subscribe()
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
    pub async fn open_slot(&self, room_id: &str) -> Result<(), CallControllerError> {
        let account_id = self.ready_account_id().await?;
        if !matches!(*self.shared.slot.lock().await, CallSlot::Idle) {
            return Err(CallControllerError::AlreadyBusy);
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
        timeout(
            JOIN_TIMEOUT,
            rtc.open_slot(room_id, "m.call#room", encryption.is_encrypted()),
        )
        .await
        .map_err(|_| CallControllerError::TimedOut("opening MatrixRTC call slot"))??;
        self.ensure_account(&account_id).await
    }

    /// Join an already-open `m.call#room` slot and start default microphone
    /// capture plus playback of subscribed remote microphones.
    pub async fn start_audio(&self, room_id: &str) -> Result<CallInfo, CallControllerError> {
        let account =
            self.manager
                .active_account()
                .await
                .ok_or(CallControllerError::Unavailable(
                    AudioCallCapability::SignInRequired,
                ))?;
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
            });
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

    /// Compatibility name for the initial two-person proof. This now receives
    /// and mixes subscribed microphones from a group call too.
    pub async fn start_two_person_audio(
        &self,
        room_id: &str,
    ) -> Result<CallInfo, CallControllerError> {
        self.start_audio(room_id).await
    }

    /// Cancel capture and wait until the MatrixRTC leave has finished. If a
    /// leave fails, the first call returns the error and keeps account changes
    /// blocked. A later explicit call acknowledges that error and clears the
    /// block. The upstream leave consumes its call handle even on failure, so
    /// there is no safe way to retry that same call through `RtcService`.
    pub async fn end_call(&self) -> Result<(), CallControllerError> {
        let (cancel, mut done, token, account_id, room_id) = {
            let mut slot = self.shared.slot.lock().await;
            let running = match &mut *slot {
                CallSlot::Idle => return Ok(()),
                CallSlot::CleanupFailed { .. } => {
                    *slot = CallSlot::Idle;
                    self.shared.status.send_replace(CallStatus::Idle);
                    return Ok(());
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

    async fn mark_cleanup_unknown(
        &self,
        token: u64,
        account_id: &str,
        room_id: &str,
        reason: String,
    ) {
        let mut slot = self.shared.slot.lock().await;
        if matches!(&*slot, CallSlot::Running(running) if running.token == token) {
            *slot = CallSlot::CleanupFailed { token };
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

struct CallWorker {
    manager: Arc<AccountManager>,
    shared: Arc<Shared>,
    account_id: String,
    room_id: String,
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
            let leave = leave_with_timeout(&mut rtc, &self.room_id).await;
            self.send_ready(Err(CallControllerError::Cancelled));
            self.finish(leave.map_err(|error| error.to_string()), None)
                .await;
            return;
        }
        {
            let mut slot = self.shared.slot.lock().await;
            match &mut *slot {
                CallSlot::Running(running)
                    if running.token == self.token && running.cancel.is_some() =>
                {
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
            let audio = rtc.run_audio(&self.room_id);
            tokio::pin!(audio);
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
                    result = &mut audio => break result.map_err(CallControllerError::Media),
                }
            }
        };
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
            let join = timeout(JOIN_TIMEOUT, rtc.join(&room_id, CallOptions::default()));
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
        {
            let mut slot = self.shared.slot.lock().await;
            if matches!(&*slot, CallSlot::Running(running) if running.token == self.token)
                || matches!(&*slot, CallSlot::CleanupFailed { token } if *token == self.token)
            {
                if let Err(reason) = &cleanup {
                    *slot = CallSlot::CleanupFailed { token: self.token };
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
