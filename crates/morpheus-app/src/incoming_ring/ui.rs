//! Slint callbacks carry an opaque offer key; the actor owns its destination.
//! All joins run under the same account-transition lock as account switching.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use morpheus_matrix::AccountSelection;
use morpheus_rtc::{AudioDeviceSelection, CallInfo};
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::{runtime::Handle, sync::mpsc, task::JoinHandle};

use super::{
    audio::IncomingAudio,
    worker::{IncomingHandle, IncomingWorker},
};
use crate::{AppWindow, IncomingCallItem, audio_devices, calls::CallController, live::LiveUi};

struct AnswerCommand {
    key: String,
    selection: AccountSelection,
    audio: AudioDeviceSelection,
}

/// Owned by main until application shutdown. No UI task survives this owner.
pub(crate) struct IncomingUi {
    worker: IncomingWorker,
    stopped: Arc<AtomicBool>,
    presentation: Option<JoinHandle<()>>,
    answer: Option<JoinHandle<()>>,
    audio: Arc<IncomingAudio>,
    audio_task: Option<JoinHandle<()>>,
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
                                        // A just-answered join may already be recovering.
                                        if matches!(&current, crate::calls::CallStatus::Joined { account_id, call }
                                            | crate::calls::CallStatus::Reconnecting { account_id, call, .. }
                                            if account_id == selection.account_id() && call == &info)
                                        {
                                            // Show controls for the exact answered call.
                                            // A queued success cannot reopen an ended or
                                            // superseded call, or use the selected room.
                                            let display = crate::call_room_display(
                                                &info.room_id,
                                                ui.get_native_call_room_id().as_str(),
                                                ui.get_native_call_room_display().as_str(),
                                                ui.get_active_room_id().as_str(),
                                                ui.get_active_room_name().as_str(),
                                            );
                                            ui.set_native_call_room_id(info.room_id.into());
                                            ui.set_native_call_room_display(display.into());
                                            ui.set_native_call_status(crate::call_status_label(&current).into());
                                            ui.set_call_connected(true);
                                            ui.set_native_call_joined(true);
                                            ui.set_native_leave_unconfirmed(false);
                                            ui.set_incoming_call_notice("".into());
                                            crate::matrix_links::clear(&ui);
                                            crate::thread_browser::clear(&ui);
                                            ui.set_page(2);
                                        } else {
                                            ui.set_incoming_call_notice("The call is no longer active.".into());
                                        }
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
                // Dismiss withdraws the local banner. It sends no decline.
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
            audio,
            audio_task: Some(audio_task),
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
    }

    pub(crate) async fn finish(&mut self) {
        self.shutdown();
        if let Some(task) = self.presentation.take() {
            let _ = task.await;
        }
        if let Some(task) = self.answer.take() {
            let _ = task.await;
        }
        if let Some(task) = self.audio_task.take() {
            let _ = task.await;
        }
        self.worker.finish().await;
    }
}

impl Drop for IncomingUi {
    fn drop(&mut self) {
        self.shutdown();
    }
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

/// Bound untrusted identity labels and remove direction/format controls.
fn label(value: &str) -> String {
    value.chars().filter(|c| {
        !c.is_control()
            && !matches!(*c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
    }).take(255).collect()
}

#[cfg(test)]
mod tests {
    use super::label;

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
}
