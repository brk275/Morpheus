//! Actual actor intake and accepted-call startup against the private fixture.
//! No candidate/event ID is injected into the worker. Explicit absent device
//! IDs exercise the production receive-only path; this is not audio capture QA.
//! Quiet facts sample current cache/media state; they are not a complete history
//! of notification sends. Joined after SDK receipt removal is an immediate
//! state observation, not an acknowledgement that actor reconciliation ran.

use super::*;
use crate::{
    calls::{AudioFailure, AudioUpdate, CallController, CallControllerError, CallStatus},
    incoming_ring::{
        controller::{ControllerFailure, OfferPresentation},
        worker::{IncomingHandle, IncomingWorker},
    },
    local_video::CapturePhase,
    updater::UpdateActivity,
};
use matrix_sdk::ruma::api::client::state::get_state_events;
use morpheus_config::RoomCallMode;
use morpheus_rtc::AudioDeviceSelection;
use std::{sync::Arc, thread::JoinHandle};
use tokio::sync::{Mutex, oneshot};

const OBSERVE_LIMIT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy)]
pub(super) enum ActorStage {
    WorkerStart,
    FirstDiscovery,
    KeyRejection,
    WorkerWithdrawal,
    FreshOffer,
    AnswerReservation,
    TransitionLock,
    AcceptedJoin,
    ExactJoinedIdentity,
    ReceiveOnlyState,
    EstablishedWithdrawal,
    AcceptedLeave,
    AccountRevocation,
    ActorCleanup,
}
impl ActorStage {
    fn name(self) -> &'static str {
        match self {
            Self::WorkerStart => "worker_start",
            Self::FirstDiscovery => "first_discovery",
            Self::KeyRejection => "key_rejection",
            Self::WorkerWithdrawal => "worker_withdrawal",
            Self::FreshOffer => "fresh_offer",
            Self::AnswerReservation => "answer_reservation",
            Self::TransitionLock => "transition_lock",
            Self::AcceptedJoin => "accepted_join",
            Self::ExactJoinedIdentity => "exact_joined_identity",
            Self::ReceiveOnlyState => "receive_only_state",
            Self::EstablishedWithdrawal => "established_withdrawal",
            Self::AcceptedLeave => "accepted_leave",
            Self::AccountRevocation => "account_revocation",
            Self::ActorCleanup => "actor_cleanup",
        }
    }
}
fn actor_fact(name: &'static str) {
    eprintln!("\nMORPHEUS_INCOMING_CHECK {name}=true");
}
fn error(category: &'static str) {
    eprintln!("\nMORPHEUS_INCOMING_ERROR {category}");
}
fn check(value: bool) -> Result<(), ()> {
    if value {
        Ok(())
    } else {
        error("assertion");
        Err(())
    }
}
fn controller_error(value: ControllerFailure) {
    error(match value {
        ControllerFailure::Unavailable => "unavailable",
        ControllerFailure::Capacity => "capacity",
        ControllerFailure::Stale => "stale",
        ControllerFailure::Expired => "expired",
        ControllerFailure::Busy => "busy",
        ControllerFailure::Revoked => "revoked",
        ControllerFailure::WrongCall => "wrong_call",
        ControllerFailure::Admission(value) => match value {
            AdmissionFailure::Account => "admission_account",
            AdmissionFailure::Sync => "admission_sync",
            AdmissionFailure::Receipt => "admission_receipt",
            AdmissionFailure::Replay => "admission_replay",
            AdmissionFailure::Expired => "admission_expired",
            AdmissionFailure::Membership => "admission_membership",
            AdmissionFailure::Trust => "admission_trust",
            AdmissionFailure::Policy => "admission_policy",
            AdmissionFailure::Quiet => "admission_quiet",
            AdmissionFailure::Transport => "admission_transport",
            AdmissionFailure::Unavailable => "admission_unavailable",
            AdmissionFailure::TimedOut => "admission_timed_out",
            AdmissionFailure::Revoked => "admission_revoked",
        },
    });
}
fn call_error(value: CallControllerError) {
    error(match value {
        CallControllerError::Unavailable(_) => "unavailable",
        CallControllerError::AlreadyBusy => "busy",
        CallControllerError::Cancelled => "cancelled",
        CallControllerError::AccountChanged => "account_changed",
        CallControllerError::StaleCall => "stale",
        CallControllerError::IncomingOfferUnavailable => "incoming_offer_unavailable",
        CallControllerError::RoomNotLoaded => "room_not_loaded",
        CallControllerError::UnknownEncryptionState => "encryption_state",
        CallControllerError::RoomState(_) => "room_state",
        CallControllerError::Matrix(_) => "matrix",
        CallControllerError::Rtc(_) => "rtc",
        CallControllerError::Media(_) => "media",
        CallControllerError::WorkerStopped => "worker_stopped",
        CallControllerError::TimedOut(_) => "timed_out",
        CallControllerError::Cleanup(_) => "cleanup",
        CallControllerError::Update(_) => "update",
    });
}
async fn bounded<T>(future: impl Future<Output = Result<T, ()>>) -> Result<T, ()> {
    time::timeout(OBSERVE_LIMIT, future)
        .await
        .map_err(|_| error("timed_out"))?
}

pub(super) struct ActorAcceptance {
    worker: IncomingWorker,
    calls: Arc<CallController>,
    pub(super) preferences: Arc<VoiceSpaces>,
    manager: Arc<AccountManager>,
    account: AccountInfo,
    transition: Mutex<()>,
    devices: AudioDeviceSelection,
    device_probe: Option<JoinHandle<()>>,
    receipt: Option<InvitationReceipt>,
    stage: ActorStage,
    first_key: Option<String>,
}
impl ActorAcceptance {
    pub(super) fn new(
        case: &Case,
        manager: Arc<AccountManager>,
        account: AccountInfo,
    ) -> Result<Self, ()> {
        let preferences = Arc::new(VoiceSpaces::new(morpheus_config::ConfigStore::at(
            case.profile.path().join("incoming-config.toml"),
        )));
        // Set this before actor registration. Answer must use the admitted
        // dialect, not an account/room's different saved outgoing preference.
        preferences
            .set_room_call_mode(
                &account.id,
                case.room_id.as_str(),
                match case.mode {
                    InteropMode::Current => RoomCallMode::ElementCallMatrix2,
                    InteropMode::Sticky2025 => RoomCallMode::Standard,
                    InteropMode::RoomState => return Err(()),
                },
            )
            .map_err(|_| error("unavailable"))?;
        let worker = IncomingWorker::start(
            &tokio::runtime::Handle::current(),
            manager.clone(),
            preferences.clone(),
        );
        let calls = Arc::new(CallController::new(
            manager.clone(),
            UpdateActivity::default(),
        ));
        let unavailable = format!("alsa:morpheus-fixture-unavailable-{}", account.id);
        let mut result = Self {
            worker,
            calls,
            preferences,
            manager,
            account,
            transition: Mutex::new(()),
            devices: AudioDeviceSelection {
                microphone: Some(unavailable.clone()),
                speaker: Some(unavailable),
            },
            device_probe: None,
            receipt: None,
            stage: ActorStage::WorkerStart,
            first_key: None,
        };
        result.phase(ActorStage::WorkerStart);
        Ok(result)
    }
    fn phase(&mut self, phase: ActorStage) {
        self.stage = phase;
        eprintln!("\nMORPHEUS_INCOMING_STAGE {}", phase.name());
    }
    pub(super) fn report_failure(&self) {
        eprintln!("\nMORPHEUS_INCOMING_FAILURE {}", self.stage.name());
    }
    pub(super) async fn prepare_devices(&mut self) -> Result<(), ()> {
        // A single owned OS thread keeps a stuck host enumeration out of the
        // Tokio blocking pool. A probe not finished at cleanup fails the gate.
        let (send, receive) = oneshot::channel();
        self.device_probe = Some(
            std::thread::Builder::new()
                .name("fixture-audio-catalog".into())
                .spawn(move || {
                    let _ = send.send(morpheus_rtc::audio_devices());
                })
                .map_err(|_| error("media"))?,
        );
        let catalog = bounded(async {
            receive
                .await
                .map_err(|_| error("media"))?
                .map_err(|_| error("media"))
        })
        .await?;
        let absent = self.devices.microphone.as_deref().ok_or(())?;
        check(
            catalog
                .microphones
                .iter()
                .chain(catalog.speakers.iter())
                .all(|device| device.id != absent),
        )
    }
    pub(super) async fn first_discovery(
        &mut self,
        case: &Case,
        caller: &AccountInfo,
    ) -> Result<(), ()> {
        self.phase(ActorStage::FirstDiscovery);
        let presentation = wait_offer(&self.worker.handle(), &self.account, caller, case).await?;
        let key = presentation.token.ui_key();
        self.first_key = Some(key.clone());
        actor_fact("actor_discovered");
        self.phase(ActorStage::KeyRejection);
        for wrong in ["0".to_owned(), format!("{key}-different")] {
            check(matches!(
                self.worker.handle().answer(wrong).await,
                Err(ControllerFailure::Stale)
            ))?;
        }
        check(
            self.worker
                .handle()
                .snapshot()
                .iter()
                .any(|offer| offer.token.ui_key() == key),
        )
    }
    pub(super) async fn first_withdrawal(&mut self, original_deadline: Instant) -> Result<(), ()> {
        self.phase(ActorStage::WorkerWithdrawal);
        let key = self.first_key.as_deref().ok_or(())?;
        wait_absent(&self.worker.handle(), key, original_deadline).await?;
        check(self.worker.handle().answer(key.to_owned()).await.is_err())?;
        actor_fact("actor_withdrew");
        Ok(())
    }
    async fn fresh_offer(
        &mut self,
        case: &Case,
        caller: &AccountInfo,
        alice: &AccountManager,
        rtc: &RtcService,
        info: &CallInfo,
        bob_room: &Room,
    ) -> Result<OfferPresentation, ()> {
        check(self.receipt.is_none())?;
        let selection = alice.account_selection(&caller.id).ok_or(())?;
        let active = AtomicU64::new(1);
        let stop = CaptureStop::default();
        let quiet = || false;
        let result = OutgoingRing::new(
            caller, selection, info, case.mode, 1, &active, &stop, &quiet,
        )
        .send(rtc)
        .await;
        let InvitationOutcome::Sent(receipt) = result else {
            error("unavailable");
            return Err(());
        };
        self.receipt = Some(receipt);
        let row = wait_offer(&self.worker.handle(), &self.account, caller, case).await?;
        check(
            self.first_key
                .as_ref()
                .is_none_or(|key| row.token.ui_key() != *key),
        )?;
        let receipt = self.receipt.as_ref().ok_or(())?;
        // The actor saw no sender-side event ID. Independently cross-check the
        // one fresh retained event after it has published its own presentation.
        let snapshot = bob_room.sticky_events().keyless_live();
        check(
            !snapshot.invalidated
                && snapshot.events.len() == 1
                && snapshot.events[0].event_id == receipt.event_id,
        )?;
        Ok(row)
    }
    pub(super) async fn answer_and_leave(
        &mut self,
        case: &Case,
        caller: &AccountInfo,
        alice: &AccountManager,
        rtc: &RtcService,
        info: &CallInfo,
        bob_room: &Room,
        observer: &mut PassiveCallObserver,
    ) -> Result<(), ()> {
        self.phase(ActorStage::FreshOffer);
        let row = self
            .fresh_offer(case, caller, alice, rtc, info, bob_room)
            .await?;
        let key = row.token.ui_key();
        let slot = server_slot(rtc.client(), &case.room_id).await?;
        let mut quiet = QuietEvidence::new(rtc.client().get_room(&case.room_id).ok_or(())?)?;
        self.phase(ActorStage::AnswerReservation);
        let accepted = self
            .worker
            .handle()
            .answer(key.clone())
            .await
            .map_err(controller_error)?;
        check(matches!(
            self.worker.handle().answer(key).await,
            Err(ControllerFailure::Stale)
        ))?;
        actor_fact("exact_key_once");
        let target = accepted.target().map_err(controller_error)?;
        check(
            target.selection().account_id() == self.account.id
                && target.room_id() == case.room_id.as_str()
                && target.options().slot_id == "m.call#room"
                && target.options().interop_mode == case.mode
                && self
                    .preferences
                    .room_call_mode(&self.account.id, case.room_id.as_str())
                    != match case.mode {
                        InteropMode::Current => RoomCallMode::Standard,
                        InteropMode::Sticky2025 => RoomCallMode::ElementCallMatrix2,
                        InteropMode::RoomState => return Err(()),
                    },
        )?;
        actor_fact("admitted_target_preserved");
        self.phase(ActorStage::TransitionLock);
        let pending = accepted.pending_guard();
        let transition = tokio::select! {
            biased;
            _ = pending.cancelled() => { error("revoked"); return Err(()); },
            lock = self.transition.lock() => lock,
        };
        // This is the actual production worker, not a test-side RtcService join.
        // Keep phase mutation separate from the borrowed transition guard.
        self.stage = ActorStage::AcceptedJoin;
        eprintln!("\nMORPHEUS_INCOMING_STAGE accepted_join");
        let joined = self
            .calls
            .start_accepted_audio(
                accepted,
                self.devices.clone(),
                self.preferences.clone(),
                &transition,
            )
            .await
            .map_err(call_error)?;
        drop(transition);
        self.phase(ActorStage::ExactJoinedIdentity);
        check(
            joined.room_id == case.room_id.as_str()
                && joined.slot_id == "m.call#room"
                && !joined.membership_id.is_empty(),
        )?;
        wait_joined(
            observer,
            bob_room,
            &self.account,
            caller,
            &joined,
            info,
            case.mode,
            rtc,
        )
        .await?;
        check(
            matches!(self.calls.status(), CallStatus::Joined { account_id, call } if account_id == self.account.id && call == joined),
        )?;
        actor_fact("accepted_join_identity");
        check(server_slot(rtc.client(), &case.room_id).await? == slot)?;
        actor_fact("slot_unchanged");
        self.phase(ActorStage::ReceiveOnlyState);
        let mut audio = self.calls.subscribe_audio();
        bounded(async {
            loop {
                match *audio.borrow_and_update() {
                    AudioUpdate::Failed {
                        token,
                        reason: AudioFailure::MicrophoneUnavailable,
                    } if token == self.calls.audio_token() => return Ok(()),
                    AudioUpdate::Failed { .. } | AudioUpdate::Applied { .. } => {
                        error("media");
                        return Err(());
                    }
                    _ => {}
                }
                audio.changed().await.map_err(|_| error("media"))?;
            }
        })
        .await?;
        self.assert_receive_only(rtc, &joined, &mut quiet)?;
        // The production accepted path selects Quiet; these observations prove
        // only sampled cache/media state, not absence of every transient send.
        actor_fact("quiet_receive_only");
        self.phase(ActorStage::EstablishedWithdrawal);
        self.redact_observed(rtc, bob_room).await?;
        check(
            matches!(self.calls.status(), CallStatus::Joined { account_id, call } if account_id == self.account.id && call == joined),
        )?;
        self.assert_receive_only(rtc, &joined, &mut quiet)?;
        // The SDK cache has removed the receipt. No actor-processing barrier
        // is available here: this is Joined at this sampled point. Deterministic
        // lease tests separately cover established authorization handoff.
        actor_fact("established_after_withdrawal");
        let expiry = current_expiry(bob_room, &self.account, &joined, case.mode)?;
        self.phase(ActorStage::AcceptedLeave);
        self.calls.end_call().await.map_err(call_error)?;
        wait_left(
            observer,
            bob_room,
            &self.account,
            caller,
            &joined,
            info,
            case.mode,
            expiry,
        )
        .await?;
        check(
            self.calls.status() == CallStatus::Idle && !self.calls.status().blocks_account_switch(),
        )?;
        quiet.check(&self.account.user_id)?;
        actor_fact("accepted_leave_observed");
        if case.mode == InteropMode::Current && case.dm {
            self.account_aba(case, caller, alice, rtc, info, bob_room, observer)
                .await?;
        }
        Ok(())
    }
    fn assert_receive_only(
        &self,
        rtc: &RtcService,
        joined: &CallInfo,
        quiet: &mut QuietEvidence,
    ) -> Result<(), ()> {
        let video = self.calls.subscribe_local_video().borrow().clone();
        check(
            video.token == self.calls.video_token()
                && video.camera.phase == CapturePhase::Off
                && video.screen.phase == CapturePhase::Off
                && self.calls.invitation().text.is_empty(),
        )?;
        let participants = rtc
            .participants(&joined.room_id)
            .map_err(|_| error("rtc"))?;
        let own = participants
            .iter()
            .find(|member| {
                member.user_id == self.account.user_id
                    && member.device_id.as_deref() == Some(self.account.device_id.as_str())
                    && member.member_id == joined.membership_id
            })
            .ok_or_else(|| error("wrong_call"))?;
        check(own.streams.iter().all(|stream| {
            !matches!(
                stream.kind,
                RtcStreamKind::Microphone
                    | RtcStreamKind::Camera
                    | RtcStreamKind::ScreenShare
                    | RtcStreamKind::ScreenShareAudio
            )
        }))?;
        quiet.check(&self.account.user_id)
    }
    async fn redact_observed(&mut self, rtc: &RtcService, room: &Room) -> Result<(), ()> {
        let receipt = self.receipt.as_ref().ok_or(())?;
        let mut updates = room.sticky_events().subscribe_keyless();
        let before = room.sticky_events().keyless_live();
        let original = before
            .events
            .iter()
            .find(|event| event.event_id == receipt.event_id)
            .ok_or(())?;
        check(!before.invalidated && MilliSecondsSinceUnixEpoch::now() < original.expires_at)?;
        check(receipt.redact(rtc).await == RetractionOutcome::Accepted)?;
        bounded(async {
            loop {
                drain(&mut updates, before.generation, |update| {
                    (update.generation, update.invalidated)
                })?;
                let snapshot = room.sticky_events().keyless_live();
                check(
                    !snapshot.invalidated
                        && snapshot.generation == before.generation
                        && MilliSecondsSinceUnixEpoch::now() < original.expires_at,
                )?;
                if snapshot
                    .events
                    .iter()
                    .all(|event| event.event_id != receipt.event_id)
                {
                    return Ok(());
                }
                updates.recv().await.map_err(|_| error("revoked"))?;
            }
        })
        .await?;
        self.receipt = None;
        Ok(())
    }
    async fn account_aba(
        &mut self,
        case: &Case,
        caller: &AccountInfo,
        alice: &AccountManager,
        rtc: &RtcService,
        info: &CallInfo,
        room: &Room,
        observer: &mut PassiveCallObserver,
    ) -> Result<(), ()> {
        self.phase(ActorStage::AccountRevocation);
        let row = self
            .fresh_offer(case, caller, alice, rtc, info, room)
            .await?;
        let key = row.token.ui_key();
        let accepted = self
            .worker
            .handle()
            .answer(key.clone())
            .await
            .map_err(controller_error)?;
        let selected = accepted.selection().clone();
        let pending = accepted.pending_guard();
        let fixture = case.account("a_primary")?;
        let alternate = self
            .manager
            .add_password_account(
                &fixture.homeserver_url,
                &fixture.username,
                &fixture.password,
            )
            .await
            .map_err(|_| error("matrix"))?;
        check(
            alternate.id != self.account.id
                && !selected.is_current()
                && self.worker.handle().snapshot().is_empty(),
        )?;
        bounded(async {
            pending.cancelled().await;
            Ok(())
        })
        .await?;
        self.manager
            .switch_to(&self.account.id)
            .await
            .map_err(|_| error("matrix"))?;
        check(!selected.is_current() && self.worker.handle().answer(key).await.is_err())?;
        let transition = self.transition.lock().await;
        check(
            self.calls
                .start_accepted_audio(
                    accepted,
                    self.devices.clone(),
                    self.preferences.clone(),
                    &transition,
                )
                .await
                .is_err(),
        )?;
        drop(transition);
        check(self.calls.status() == CallStatus::Idle)?;
        let snapshot = observer.snapshot().await.map_err(|_| error("rtc"))?;
        check(
            snapshot.participants.len() == 1
                && snapshot.participants[0].user_id == caller.user_id
                && snapshot.participants[0].membership_id == info.membership_id,
        )?;
        // Keep the third known receipt for unconditional cleanup. The restored
        // account's old keyless lifetime can be invalidated by its sync restart.
        actor_fact("account_replay_rejected");
        Ok(())
    }
    pub(super) fn shutdown(&self) {
        self.worker.shutdown();
    }
    pub(super) async fn cleanup_call(&mut self) -> bool {
        self.phase(ActorStage::ActorCleanup);
        self.shutdown();
        let calls = self.calls.clone();
        let ((), ended) = tokio::join!(
            self.worker.finish(),
            cleanup_attempt(Duration::from_secs(75), async move {
                calls.end_call().await.is_ok()
            })
        );
        let probe = match self.device_probe.take() {
            Some(probe) if probe.is_finished() => probe.join().is_ok(),
            Some(_) => false,
            None => true,
        };
        ended
            && probe
            && self.calls.status() == CallStatus::Idle
            && self.worker.handle().snapshot().is_empty()
    }
    pub(super) fn take_receipt(&mut self) -> Option<InvitationReceipt> {
        self.receipt.take()
    }
}

async fn wait_offer(
    handle: &IncomingHandle,
    own: &AccountInfo,
    caller: &AccountInfo,
    case: &Case,
) -> Result<OfferPresentation, ()> {
    let mut updates = handle.subscribe();
    bounded(async {
        loop {
            let rows = handle.snapshot();
            check(rows.len() <= 1)?;
            if let Some(row) = rows.into_iter().next() {
                check(
                    row.account_id == own.id
                        && row.room_id == case.room_id.as_str()
                        && row.caller_user_id == caller.user_id
                        && row.mode == case.mode,
                )?;
                return Ok(row);
            }
            updates.changed().await.map_err(|_| error("unavailable"))?;
        }
    })
    .await
}
async fn wait_absent(handle: &IncomingHandle, key: &str, deadline: Instant) -> Result<(), ()> {
    let mut updates = handle.subscribe();
    bounded(async {
        loop {
            check(Instant::now() < deadline)?;
            if handle
                .snapshot()
                .iter()
                .all(|offer| offer.token.ui_key() != key)
            {
                return Ok(());
            }
            updates.changed().await.map_err(|_| error("unavailable"))?;
        }
    })
    .await
}
async fn server_slot(client: &Client, room: &OwnedRoomId) -> Result<Value, ()> {
    let result = client
        .send(get_state_events::v3::Request::new(room.clone()))
        .with_request_config(
            RequestConfig::new()
                .retry_limit(0)
                .timeout(Duration::from_secs(5)),
        )
        .await
        .map_err(|_| error("room_state"))?;
    check(result.room_state.len() <= 512)?;
    let mut found = None;
    for event in result.room_state {
        check(event.json().get().len() <= MAX_MEMBERSHIP_BYTES)?;
        let value: Value =
            serde_json::from_str(event.json().get()).map_err(|_| error("room_state"))?;
        if value.get("type").and_then(Value::as_str) == Some("m.rtc.slot")
            && value.get("state_key").and_then(Value::as_str) == Some("m.call#room")
        {
            check(found.is_none() && value.get("event_id").and_then(Value::as_str).is_some())?;
            // Unsigned server metadata can vary. Identity and content may not.
            found = Some(
                serde_json::json!({"event_id": value.get("event_id"), "content":value.get("content")}),
            );
        }
    }
    found.ok_or_else(|| error("room_state"))
}
async fn wait_joined(
    observer: &mut PassiveCallObserver,
    room: &Room,
    own: &AccountInfo,
    caller: &AccountInfo,
    joined: &CallInfo,
    caller_info: &CallInfo,
    mode: InteropMode,
    rtc: &RtcService,
) -> Result<(), ()> {
    let own_dialect = NotificationDialect::new(
        mode,
        &own.user_id,
        &own.device_id,
        &joined.room_id,
        &joined.slot_id,
    )
    .ok_or(())?;
    let caller_dialect = NotificationDialect::new(
        mode,
        &caller.user_id,
        &caller.device_id,
        &caller_info.room_id,
        &caller_info.slot_id,
    )
    .ok_or(())?;
    bounded(async {
        loop {
            let snapshot = observer.snapshot().await.map_err(|_| error("rtc"))?;
            check(
                snapshot.room_id == joined.room_id
                    && snapshot.slot_id == joined.slot_id
                    && snapshot.mode == mode
                    && snapshot.participants.len() <= 2,
            )?;
            if snapshot.participants.len() == 2 {
                let mut exact = true;
                for (account, info, dialect) in [
                    (own, joined, &own_dialect),
                    (caller, caller_info, &caller_dialect),
                ] {
                    let matching = snapshot
                        .participants
                        .iter()
                        .filter(|member| {
                            member.user_id == account.user_id
                                && member.device_id == account.device_id
                                && member.membership_id == info.membership_id
                        })
                        .collect::<Vec<_>>();
                    check(matching.len() == 1)?;
                    exact &= own_membership(room, account, info, dialect, mode)
                        .map_err(|_| error("admission_trust"))?
                        .is_some_and(|event| event.as_str() == matching[0].membership_event_id);
                }
                // The peer's native roster is delivered independently from
                // Bob's passive SDK feed. Require its actual exact generation
                // too, without treating a not-yet-arrived peer as success.
                let peer = rtc
                    .participants(&joined.room_id)
                    .map_err(|_| error("rtc"))?;
                check(peer.len() <= 2)?;
                let visible = peer
                    .iter()
                    .filter(|member| {
                        member.user_id == own.user_id
                            && member.device_id.as_deref() == Some(own.device_id.as_str())
                            && member.member_id == joined.membership_id
                            && !member.is_local
                    })
                    .count()
                    == 1;
                if exact && visible {
                    return Ok(());
                }
            }
            time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
}
fn current_expiry(
    room: &Room,
    own: &AccountInfo,
    info: &CallInfo,
    mode: InteropMode,
) -> Result<MilliSecondsSinceUnixEpoch, ()> {
    let dialect = NotificationDialect::new(
        mode,
        &own.user_id,
        &own.device_id,
        &info.room_id,
        &info.slot_id,
    )
    .ok_or(())?;
    let id = own_membership(room, own, info, &dialect, mode)
        .map_err(|_| error("admission_trust"))?
        .ok_or(())?;
    room.sticky_events()
        .live()
        .into_iter()
        .find(|event| event.event_id == id)
        .map(|event| event.expires_at)
        .ok_or(())
}
async fn wait_left(
    observer: &mut PassiveCallObserver,
    room: &Room,
    own: &AccountInfo,
    caller: &AccountInfo,
    joined: &CallInfo,
    caller_info: &CallInfo,
    mode: InteropMode,
    expiry: MilliSecondsSinceUnixEpoch,
) -> Result<(), ()> {
    let dialect = NotificationDialect::new(
        mode,
        &own.user_id,
        &own.device_id,
        &joined.room_id,
        &joined.slot_id,
    )
    .ok_or(())?;
    bounded(async {
        loop {
            check(MilliSecondsSinceUnixEpoch::now() < expiry)?;
            let snapshot = observer.snapshot().await.map_err(|_| error("rtc"))?;
            check(
                snapshot.room_id == joined.room_id
                    && snapshot.slot_id == joined.slot_id
                    && snapshot.mode == mode,
            )?;
            if snapshot.participants.len() == 1
                && snapshot.participants[0].user_id == caller.user_id
                && snapshot.participants[0].device_id == caller.device_id
                && snapshot.participants[0].membership_id == caller_info.membership_id
                && own_membership(room, own, joined, &dialect, mode)
                    .map_err(|_| error("admission_trust"))?
                    .is_none()
            {
                return Ok(());
            }
            time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
}
fn drain<T: Clone>(
    updates: &mut tokio::sync::broadcast::Receiver<T>,
    generation: u64,
    fields: impl Fn(&T) -> (u64, bool),
) -> Result<(), ()> {
    for _ in 0..64 {
        match updates.try_recv() {
            Ok(update) => {
                let (current, invalidated) = fields(&update);
                check(current == generation && !invalidated)?;
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => return Ok(()),
            Err(_) => {
                error("revoked");
                return Err(());
            }
        }
    }
    error("capacity");
    Err(())
}
// Keyless wakeups are not a complete delta log. A notification sent and removed
// between snapshots may be absent here; lag/generation failures still fail closed.
struct QuietEvidence {
    room: Room,
    generation: u64,
    drain: Box<dyn FnMut() -> Result<(), ()> + Send>,
}
impl QuietEvidence {
    fn new(room: Room) -> Result<Self, ()> {
        let mut updates = room.sticky_events().subscribe_keyless();
        let snapshot = room.sticky_events().keyless_live();
        check(!snapshot.invalidated)?;
        let generation = snapshot.generation;
        Ok(Self {
            room,
            generation,
            drain: Box::new(move || {
                drain(&mut updates, generation, |update| {
                    (update.generation, update.invalidated)
                })
            }),
        })
    }
    fn check(&mut self, receiver: &str) -> Result<(), ()> {
        (self.drain)()?;
        let snapshot = self.room.sticky_events().keyless_live();
        check(
            !snapshot.invalidated
                && snapshot.generation == self.generation
                && snapshot
                    .events
                    .iter()
                    .all(|event| event.sender.as_str() != receiver),
        )
    }
}
