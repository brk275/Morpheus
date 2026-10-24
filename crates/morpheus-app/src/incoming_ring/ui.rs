//! Slint callbacks carry an opaque offer key; the actor owns its destination.
//! All joins run under the same account-transition lock as account switching.
//! Dismiss only removes the offer on this device. Decline also sends one
//! bounded protocol decline (`IncomingOffer::decline`) and reports its
//! settled outcome; it never joins, captures media or changes permissions.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use morpheus_matrix::AccountSelection;
use morpheus_rtc::{AudioDeviceSelection, CallInfo};
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::{runtime::Handle, sync::mpsc, task::JoinHandle};

pub(crate) use super::worker::IncomingDrainError;
use super::{
    DeclineOutcome,
    audio::IncomingAudio,
    worker::{IncomingHandle, IncomingWorker},
};
use crate::{AppWindow, IncomingCallItem, audio_devices, calls::CallController, live::LiveUi};

struct AnswerCommand {
    key: String,
    selection: AccountSelection,
    audio: AudioDeviceSelection,
}

/// One Decline click: the offer key plus the selection and security
/// generation captured at the click. Never refreshed for a later attempt.
struct DeclineCommand {
    key: String,
    selection: AccountSelection,
    security: u64,
}

/// Owned by main until application shutdown. No UI task survives this owner.
pub(crate) struct IncomingUi {
    worker: IncomingWorker,
    stopped: Arc<AtomicBool>,
    presentation: Option<JoinHandle<()>>,
    answer: Option<JoinHandle<()>>,
    decline: Option<JoinHandle<()>>,
    audio: Arc<IncomingAudio>,
    audio_task: Option<JoinHandle<()>>,
    failure: Result<(), IncomingDrainError>,
    completion: Option<Result<(), IncomingDrainError>>,
}

impl IncomingUi {
    pub(crate) fn bind(
        ui: &AppWindow,
        runtime: &Handle,
        live: Arc<LiveUi>,
        calls: Arc<CallController>,
    ) -> Self {
        let worker =
            IncomingWorker::start(runtime, live.manager.clone(), live.voice_spaces.clone());
        let handle = worker.handle();
        let stopped = Arc::new(AtomicBool::new(false));
        let (audio, audio_task) =
            IncomingAudio::start(runtime, handle.clone(), live.clone(), calls.clone());
        let (answers, mut commands) = mpsc::channel::<AnswerCommand>(1);

        let presentation = {
            let audio = audio.clone();
            let handle = handle.clone();
            let account_updates = live.manager.subscribe_active_account();
            let weak = ui.as_weak();
            let stopped = stopped.clone();
            runtime.spawn(async move {
                let mut updates = handle.subscribe();
                let mut sound_updates = audio.subscribe();
                let queued = Arc::new(AtomicBool::new(false));
                loop {
                    updates.borrow_and_update();
                    sound_updates.borrow_and_update();
                    let current = handle.clone();
                    let audio = audio.clone();
                    let account_updates = account_updates.clone();
                    let stopped = stopped.clone();
                    // A stalled native UI retains at most one refresh closure.
                    // Clear before reading current state so an update racing
                    // this render can schedule the next one without being lost.
                    if !queued.swap(true, Ordering::AcqRel) {
                        let queued = queued.clone();
                        if weak
                            .upgrade_in_event_loop(move |ui| {
                                queued.store(false, Ordering::Release);
                                if stopped.load(Ordering::Acquire) {
                                    return;
                                }
                                // Read current actor output here, not before this UI
                                // closure was queued. Do not restore a withdrawn row.
                                let account = account_updates.borrow().clone();
                                let account_id =
                                    account.as_ref().map(|a| a.id.as_str()).unwrap_or("");
                                let account_label =
                                    account.as_ref().map(|a| a.user_id.as_str()).unwrap_or("");
                                ui.set_incoming_call_account_id(account_id.into());
                                let visible: Vec<_> = current
                                    .snapshot()
                                    .into_iter()
                                    .filter(|offer| offer.account_id == account_id)
                                    .collect();
                                ui.set_incoming_call_sound_status(audio.notice(&visible).into());
                                let rows: Vec<_> = visible
                                    .into_iter()
                                    .map(|offer| IncomingCallItem {
                                        token: offer.token.ui_key().into(),
                                        caller: label(&offer.caller_user_id).into(),
                                        room: label(&offer.room_id).into(),
                                        account: label(account_label).into(),
                                    })
                                    .collect();
                                ui.set_incoming_call_offers(ModelRc::new(VecModel::from(rows)));
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    tokio::select! {
                        changed = updates.changed() => { if changed.is_err() { break; } },
                        changed = sound_updates.changed() => { if changed.is_err() { break; } },
                    }
                }
            })
        };

        let answer = {
            let audio = audio.clone();
            let handle = handle.clone();
            let live = live.clone();
            let calls = calls.clone();
            let stopped = stopped.clone();
            let weak = ui.as_weak();
            runtime.spawn(async move {
                while let Some(command) = commands.recv().await {
                    let selection = command.selection.clone();
                    let result = answer_once(&handle, &live, &calls, &stopped, command).await;
                    let audio = audio.clone();
                    let calls_for_ui = calls.clone();
                    let stopped = stopped.clone();
                    if weak
                        .upgrade_in_event_loop(move |ui| {
                            if stopped.load(Ordering::Acquire) {
                                return;
                            }
                            // This callback is the only place that releases these
                            // flags. A second UI action cannot overtake completion.
                            audio.end_answer();
                            ui.set_incoming_call_answering(false);
                            ui.set_native_call_request_pending(false);
                            if selection.is_current()
                                && ui.get_active_account_local_id().as_str()
                                    == selection.account_id()
                            {
                                match result {
                                    Ok(info) => {
                                        let current = calls_for_ui.status();
                                        show_answered_call(
                                            &ui,
                                            selection.account_id(),
                                            &current,
                                            info,
                                        );
                                    }
                                    Err(message) => ui.set_incoming_call_notice(message.into()),
                                }
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
        };

        // Decline: one bounded protocol attempt per click, off the actor. The
        // channel holds one click; the settled outcome is shown only to the
        // account that clicked, while its selection is still current.
        let (declines, mut decline_commands) = mpsc::channel::<DeclineCommand>(1);
        let decline = {
            let handle = handle.clone();
            let live = live.clone();
            let stopped = stopped.clone();
            let weak = ui.as_weak();
            runtime.spawn(async move {
                while let Some(command) = decline_commands.recv().await {
                    let selection = command.selection.clone();
                    let outcome = decline_once(&handle, &live, &stopped, command).await;
                    let stopped = stopped.clone();
                    if weak
                        .upgrade_in_event_loop(move |ui| {
                            if stopped.load(Ordering::Acquire) {
                                return;
                            }
                            // The only place that releases this flag.
                            ui.set_incoming_call_declining(false);
                            if selection.is_current()
                                && ui.get_active_account_local_id().as_str()
                                    == selection.account_id()
                            {
                                ui.set_incoming_call_notice(decline_notice(outcome).into());
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
        };
        {
            let weak = ui.as_weak();
            let stopped = stopped.clone();
            let audio = audio.clone();
            let ring_handle = handle.clone();
            let live = live.clone();
            ui.on_decline_incoming_call(move |key| {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                // Captured first: a security step that begins (and even ends)
                // before the checks below still refuses this click.
                let security = live.security_gate.ringing_generation();
                if stopped.load(Ordering::Acquire)
                    || !ui.get_live_session()
                    || ui.get_incoming_call_declining()
                    || ui.get_incoming_call_answering()
                    || !live.security_gate.ringing_allowed(security)
                {
                    return;
                }
                let Some(selection) = live
                    .manager
                    .account_selection(ui.get_active_account_local_id().as_str())
                else {
                    return;
                };
                audio.silence(&ring_handle);
                let command = DeclineCommand {
                    key: key.to_string(),
                    selection,
                    security,
                };
                if declines.try_send(command).is_err() {
                    ui.set_incoming_call_notice("That call is no longer available.".into());
                    return;
                }
                // Pending only: nothing is claimed as sent until the server
                // confirms it.
                ui.set_incoming_call_declining(true);
                ui.set_incoming_call_notice("Declining the call…".into());
            });
        }

        {
            let weak = ui.as_weak();
            let stopped = stopped.clone();
            let audio = audio.clone();
            let ring_handle = handle.clone();
            ui.on_answer_incoming_call(move |key| {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                if stopped.load(Ordering::Acquire)
                    || !ui.get_live_session()
                    || ui.get_incoming_call_answering()
                    || ui.get_native_call_request_pending()
                    || calls.status().blocks_account_switch()
                    || live.security_gate.blocked()
                {
                    return;
                }
                let Some(selection) = live
                    .manager
                    .account_selection(ui.get_active_account_local_id().as_str())
                else {
                    return;
                };
                let command = AnswerCommand {
                    key: key.to_string(),
                    selection,
                    audio: audio_devices::selection(&ui),
                };
                audio.begin_answer(&ring_handle);
                if answers.try_send(command).is_err() {
                    audio.end_answer();
                    ui.set_incoming_call_notice("That call is no longer available.".into());
                    return;
                }
                ui.set_incoming_call_notice("".into());
                ui.set_incoming_call_answering(true);
                ui.set_native_call_request_pending(true);
            });
        }
        {
            let weak = ui.as_weak();
            let stopped = stopped.clone();
            let audio = audio.clone();
            ui.on_dismiss_incoming_call(move |key| {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                if stopped.load(Ordering::Acquire) {
                    return;
                }
                audio.silence(&handle);
                // Dismiss withdraws the banner on this device only. It sends
                // no decline; the caller and other devices are not told.
                if handle.dismiss(key.to_string()).is_err() {
                    ui.set_incoming_call_notice("That call is no longer available.".into());
                }
            });
        }

        Self {
            worker,
            stopped,
            presentation: Some(presentation),
            answer: Some(answer),
            decline: Some(decline),
            audio,
            audio_task: Some(audio_task),
            failure: Ok(()),
            completion: None,
        }
    }

    /// Admitted incoming offers observed by the desktop alert owner.
    pub(crate) fn offers(&self) -> IncomingHandle {
        self.worker.handle()
    }

    /// A read-only predicate over the current account's exact-room offers.
    pub(crate) fn ringing_in(&self) -> impl Fn(&str, &str) -> bool + Send + Sync + 'static {
        let offers = self.worker.handle();
        move |account: &str, room: &str| {
            offers
                .snapshot()
                .iter()
                .any(|offer| offer.account_id == account && offer.room_id == room)
        }
    }

    /// Revoke synchronously before waiting for any other shutdown operation.
    pub(crate) fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        self.audio.shutdown();
        if let Some(task) = &self.audio_task {
            task.abort();
        }
        self.worker.shutdown();
        if let Some(task) = &self.presentation {
            task.abort();
        }
        // Dropping a pending accepted capability/startup waiter revokes its
        // worker authority; calls.end_call owns any entered join cleanup.
        if let Some(task) = &self.answer {
            task.abort();
        }
        // Dropping a Decline claim revokes it: nothing is queued, retried or
        // replayed after shutdown.
        if let Some(task) = &self.decline {
            task.abort();
        }
    }

    pub(crate) async fn finish(&mut self) {
        let _ = self.finish_checked().await;
    }

    pub(crate) async fn finish_checked(&mut self) -> Result<(), IncomingDrainError> {
        self.shutdown();
        if let Some(result) = self.completion {
            return result;
        }
        // shutdown deliberately aborts these four UI roles. Observe each
        // original handle before take, preserving a cancelled observation and
        // retaining any earlier joined failure across the next await.
        for task in [
            &mut self.presentation,
            &mut self.answer,
            &mut self.decline,
            &mut self.audio_task,
        ] {
            let joined = super::worker::join_incoming_task(task, true).await;
            self.failure = super::worker::combine_drain(self.failure, joined);
        }
        let actor = self.worker.finish_checked().await;
        self.failure = super::worker::combine_drain(self.failure, actor);
        self.completion = Some(self.failure);
        self.failure
    }
}

impl Drop for IncomingUi {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Apply only the exact answered call that is still joined or recovering.
/// The protected page entry retires pending Leave synchronously, before its
/// flag can defer cancellation to Slint's change handler.
fn show_answered_call(
    ui: &AppWindow,
    account_id: &str,
    current: &crate::calls::CallStatus,
    info: CallInfo,
) {
    // A just-answered join may already be recovering. A queued success cannot
    // reopen an ended or superseded call, or use the selected room instead.
    if matches!(current, crate::calls::CallStatus::Joined { account_id: current_account, call }
        | crate::calls::CallStatus::Reconnecting { account_id: current_account, call, .. }
        if current_account == account_id && call == &info)
    {
        let display = crate::call_room_display(
            &info.room_id,
            ui.get_native_call_room_id().as_str(),
            ui.get_native_call_room_display().as_str(),
            ui.get_active_room_id().as_str(),
            ui.get_active_room_name().as_str(),
        );
        ui.set_native_call_room_id(info.room_id.into());
        ui.set_native_call_room_display(display.into());
        ui.set_native_call_status(crate::call_status_label(current).into());
        ui.set_call_connected(true);
        ui.set_native_call_joined(true);
        ui.set_native_leave_unconfirmed(false);
        ui.set_incoming_call_notice("".into());
        crate::matrix_links::clear(ui);
        crate::thread_browser::withdraw(ui);
        ui.invoke_navigate_page(2);
    } else {
        ui.set_incoming_call_notice("The call is no longer active.".into());
    }
}

/// Existing generated-window phase only: run the actual success presentation
/// with a retained reset owner and a held cancellation-first waiter. The
/// waiter models acquisition; it is not RoomLeaveUi or a core HTTP fixture.
#[cfg(test)]
pub(crate) async fn assert_answered_call_withdraws_leave(ui: &AppWindow) {
    use crate::{
        RoomLeave, RoomLeaveView,
        calls::{CallInterruption, CallStatus},
    };
    use morpheus_matrix::RoomLeaveCancellation;
    use std::{cell::Cell, rc::Rc};

    let before = (
        ui.get_page(),
        ui.get_native_call_room_id(),
        ui.get_native_call_room_display(),
        ui.get_native_call_status(),
        ui.get_call_connected(),
        ui.get_native_call_joined(),
        ui.get_native_leave_unconfirmed(),
        ui.get_incoming_call_notice(),
        ui.global::<RoomLeave>().get_view(),
    );
    let info = CallInfo {
        room_id: "!answered-other-room:example.org".into(),
        slot_id: "m.call#room".into(),
        membership_id: "answered-membership".into(),
        local_identity: "answered-identity".into(),
    };
    for hidden in [false, true] {
        for recovering in [false, true] {
            ui.set_page(0);
            let cancellation = RoomLeaveCancellation::new();
            let retained = cancellation.clone();
            let resets = Rc::new(Cell::new(0));
            let observed = resets.clone();
            let weak = ui.as_weak();
            ui.global::<RoomLeave>().on_reset(move || {
                let ui = weak.upgrade().unwrap();
                assert_eq!(ui.get_page(), 0, "reset precedes the protected page flag");
                observed.set(observed.get() + 1);
                retained.cancel();
            });
            ui.global::<RoomLeave>().set_view(RoomLeaveView {
                open: !hidden,
                stage: 2,
                ..Default::default()
            });
            // Hold acquisition, then release it only after the actual callback
            // entry and an immediate return to the conversation. No change
            // handler is settled between those page assignments.
            let acquisition = tokio::sync::Mutex::new(());
            let hold = acquisition.lock().await;
            let dispatched = Cell::new(0);
            let waiting = async {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => false,
                    _guard = acquisition.lock() => {
                        dispatched.set(dispatched.get() + 1);
                        true
                    }
                }
            };
            tokio::pin!(waiting);
            assert!(futures_util::poll!(waiting.as_mut()).is_pending());
            let current = if recovering {
                CallStatus::Reconnecting {
                    account_id: "account-a".into(),
                    call: info.clone(),
                    interruption: CallInterruption::Signaling,
                }
            } else {
                CallStatus::Joined {
                    account_id: "account-a".into(),
                    call: info.clone(),
                }
            };
            show_answered_call(ui, "account-a", &current, info.clone());
            assert!(
                cancellation.is_cancelled(),
                "the actual entry retires even a hidden owner"
            );
            assert_eq!(resets.get(), 1);
            assert!(!ui.global::<RoomLeave>().get_view().open);
            assert_eq!(ui.get_page(), 2);
            assert_eq!(ui.get_native_call_room_id().as_str(), info.room_id.as_str());
            ui.set_page(0);
            drop(hold);
            assert!(
                !waiting.await,
                "withdrawal wins even when acquisition becomes ready too"
            );
            assert_eq!(dispatched.get(), 0);
        }
    }
    // The extracted callback keeps the full account/join CAS. Rejected
    // completions cannot withdraw another owner's Leave or change its page.
    let mut superseded = info.clone();
    superseded.membership_id = "newer-membership".into();
    for current in [
        CallStatus::Idle,
        CallStatus::Joined {
            account_id: "account-b".into(),
            call: info.clone(),
        },
        CallStatus::Joined {
            account_id: "account-a".into(),
            call: superseded,
        },
        CallStatus::CleanupFailed {
            account_id: "account-a".into(),
            room_id: info.room_id.clone(),
            reason: "fixture".into(),
        },
    ] {
        let cancellation = RoomLeaveCancellation::new();
        let retained = cancellation.clone();
        ui.global::<RoomLeave>().on_reset(move || retained.cancel());
        ui.global::<RoomLeave>().set_view(RoomLeaveView {
            open: true,
            ..Default::default()
        });
        show_answered_call(ui, "account-a", &current, info.clone());
        assert!(!cancellation.is_cancelled());
        assert!(ui.global::<RoomLeave>().get_view().open);
        assert_eq!(ui.get_page(), 0);
    }
    ui.global::<RoomLeave>().on_reset(|| {});
    ui.global::<RoomLeave>().set_view(before.8);
    ui.set_page(before.0);
    ui.set_native_call_room_id(before.1);
    ui.set_native_call_room_display(before.2);
    ui.set_native_call_status(before.3);
    ui.set_call_connected(before.4);
    ui.set_native_call_joined(before.5);
    ui.set_native_leave_unconfirmed(before.6);
    ui.set_incoming_call_notice(before.7);
}

async fn answer_once(
    handle: &IncomingHandle,
    live: &LiveUi,
    calls: &CallController,
    stopped: &AtomicBool,
    command: AnswerCommand,
) -> Result<CallInfo, &'static str> {
    const UNAVAILABLE: &str = "That call is no longer available.";
    if stopped.load(Ordering::Acquire) || !command.selection.is_current() {
        return Err(UNAVAILABLE);
    }
    let accepted = handle.answer(command.key).await.map_err(|_| UNAVAILABLE)?;
    if !command.selection.is_current()
        || accepted.selection().account_id() != command.selection.account_id()
    {
        return Err(UNAVAILABLE);
    }
    // The receipt drain inside AcceptedOffer is Send-only. Cancellation owns
    // cloned lease/selection facts; no shared borrow spans this await.
    let cancellation = accepted.pending_guard();
    let transition = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(UNAVAILABLE),
        transition = live.account_transition.lock() => transition,
    };
    if stopped.load(Ordering::Acquire)
        || !command.selection.is_current()
        || live.security_gate.blocked()
        || calls.status().blocks_account_switch()
    {
        return Err(UNAVAILABLE);
    }
    calls
        .start_accepted_audio(
            accepted,
            command.audio,
            live.voice_spaces.clone(),
            &transition,
        )
        .await
        .map_err(|_| "Could not answer this call. Check its status and your audio devices.")
}

/// One Decline click, settled. None when the offer could not be claimed (it
/// stays as it was). The click's own fence is checked again around the
/// claim, and the claim keeps it, the offer's lease and the original
/// selection for every later step. No fresh authority is ever taken.
async fn decline_once(
    handle: &IncomingHandle,
    live: &LiveUi,
    stopped: &Arc<AtomicBool>,
    command: DeclineCommand,
) -> Option<DeclineOutcome> {
    let DeclineCommand {
        key,
        selection,
        security,
    } = command;
    let allowed = {
        let stopped = stopped.clone();
        let gate = live.security_gate.clone();
        let selection = selection.clone();
        move || {
            !stopped.load(Ordering::Acquire)
                && selection.is_current()
                && gate.ringing_allowed(security)
        }
    };
    if !allowed() {
        return None;
    }
    let claim = handle.decline(key).await.ok()?;
    if !allowed() || claim.selection().account_id() != selection.account_id() {
        // Dropping the claim revokes it; nothing was sent.
        return Some(DeclineOutcome::NotSent);
    }
    Some(claim.send(&live.manager, &live.voice_spaces, allowed).await)
}

/// Plain, fixed text: no server error, room, event or account identifiers.
fn decline_notice(outcome: Option<DeclineOutcome>) -> &'static str {
    match outcome {
        Some(DeclineOutcome::Confirmed) => "Call declined.",
        Some(DeclineOutcome::NotSent) => {
            "The call was not declined: nothing was sent. It is no longer shown here."
        }
        Some(DeclineOutcome::Failed) => {
            "The call was not declined: the server did not accept it. It is no longer shown here."
        }
        Some(DeclineOutcome::Unknown) => {
            "It is not known whether the call was declined. The caller may still see it ringing."
        }
        None => "That call is no longer available.",
    }
}

/// Bound untrusted identity labels and remove direction/format controls.
fn label(value: &str) -> String {
    value.chars().filter(|c| {
        !c.is_control()
            && !matches!(*c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
    }).take(255).collect()
}

#[cfg(test)]
mod tests {
    use super::{DeclineOutcome, decline_notice, label};

    #[test]
    fn identity_labels_strip_direction_controls_and_bound_unicode_characters() {
        assert_eq!(
            label("@casey:\u{061c}example\u{202e}\u{2066}.org\n"),
            "@casey:example.org"
        );
        let long = "é".repeat(256);
        let shown = label(&long);
        assert_eq!(shown.chars().count(), 255);
        assert_eq!(shown, "é".repeat(255));
    }

    /// Only a server-confirmed send says the call was declined; every other
    /// outcome says it was not, or that it is unknown. The text is fixed:
    /// no server error, room, event or account identifier can reach it.
    #[test]
    fn decline_notices_claim_a_decline_only_when_confirmed() {
        assert_eq!(
            decline_notice(Some(DeclineOutcome::Confirmed)),
            "Call declined."
        );
        for outcome in [
            Some(DeclineOutcome::NotSent),
            Some(DeclineOutcome::Failed),
            Some(DeclineOutcome::Unknown),
            None,
        ] {
            let notice = decline_notice(outcome);
            assert_ne!(notice, "Call declined.");
            assert!(
                notice.contains("not declined")
                    || notice.contains("not known")
                    || notice.contains("no longer available"),
                "{notice}"
            );
        }
        for outcome in [
            Some(DeclineOutcome::Confirmed),
            Some(DeclineOutcome::NotSent),
            Some(DeclineOutcome::Failed),
            Some(DeclineOutcome::Unknown),
            None,
        ] {
            let notice = decline_notice(outcome);
            // No Matrix room, event, user or alias sigil.
            assert!(!notice.contains(['!', '$', '@', '#']), "{notice}");
        }
    }
}
