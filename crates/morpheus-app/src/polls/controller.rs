//! Explicit poll actions. Choosing options only edits a local draft; a vote,
//! an end or a new poll is sent once, from its own button, under the account
//! selection captured at that click. Every result is applied only while the
//! same account, room and operation are still shown; any change of account,
//! room or thread drops drafts, pending state and the dialog.

use std::{cell::RefCell, sync::Arc};

use morpheus_matrix::{
    AccountSelection, MAX_POLL_ANSWER_COUNT, MIN_POLL_ANSWER_COUNT, MessageInfo, PollDraft,
    PollError, PollSendKey,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::runtime::Handle;

use crate::{AppWindow, PollUiState, live::LiveUi};

const VOTE_SENDING: &str = "Sending your vote…";
const VOTE_SENT: &str = "Vote sent. It appears here once the server confirms it.";
const END_SENDING: &str = "Ending the poll…";
const END_SENT: &str = "End sent. The final results appear once the server confirms it.";
const ACCOUNT_CHANGED: &str = "The account changed. Nothing was sent.";

/// One poll's unsent choice.
struct Draft {
    event_id: String,
    chosen: Vec<String>,
    ready: bool,
}

/// The create dialog's target and draft, bound when it opened.
struct Dialog {
    operation: u64,
    selection: AccountSelection,
    account_id: String,
    room_id: String,
    draft: PollDraft,
    /// Reused when the same draft is sent again, so a retry after an
    /// uncertain send cannot post a second copy; any edit replaces it.
    key: PollSendKey,
    busy: bool,
}

#[derive(Default)]
struct State {
    operation: u64,
    draft: Option<Draft>,
    pending: Option<u64>,
    dialog: Option<Dialog>,
}

thread_local! {
    // Poll interaction is UI-thread state; async results return through the
    // event loop before touching it.
    static STATE: RefCell<State> = RefCell::new(State::default());
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Vote,
    End,
}

fn scope(ui: &AppWindow) -> Option<(String, String)> {
    let account = ui.get_active_account_local_id();
    let room = ui.get_active_room_id();
    (ui.get_live_session() && !account.is_empty() && !room.is_empty())
        .then(|| (account.to_string(), room.to_string()))
}

/// The live window still shows this account and room.
fn shows(ui: &AppWindow, account_id: &str, room_id: &str) -> bool {
    scope(ui).is_some_and(|(account, room)| account == account_id && room == room_id)
}

fn idle() -> bool {
    STATE.with_borrow(|state| state.pending.is_none())
}

fn dialog_busy() -> bool {
    STATE.with_borrow(|state| state.dialog.as_ref().is_some_and(|dialog| dialog.busy))
}

/// The row the person saw for this poll, in the room timeline or in the open
/// thread. Local echoes have no event ID yet and never match.
fn shown_poll(ui: &AppWindow, event_id: &str, thread_root: &str) -> Option<Arc<MessageInfo>> {
    if event_id.is_empty() {
        return None;
    }
    let model = if thread_root.is_empty() {
        ui.get_messages()
    } else if ui.get_thread_root_id().as_str() == thread_root {
        ui.get_thread_messages()
    } else {
        return None;
    };
    crate::timeline_model::info_of_event(&model, event_id)
        .filter(|info| info.poll.is_some() && !info.is_local_echo)
}

/// Apply one option press: a single-choice poll replaces the choice, a
/// multiple-choice poll toggles it within the limit. Returns false when the
/// limit refuses a new option.
fn toggle(chosen: &mut Vec<String>, option: &str, limit: usize) -> bool {
    if limit <= 1 {
        chosen.clear();
        chosen.push(option.to_owned());
        return true;
    }
    if let Some(index) = chosen.iter().position(|id| id == option) {
        chosen.remove(index);
        return true;
    }
    if chosen.len() >= limit {
        return false;
    }
    chosen.push(option.to_owned());
    true
}

/// A draft is worth sending when it names something and differs from the
/// vote already counted.
fn differs(chosen: &[String], own: &[&str]) -> bool {
    !chosen.is_empty()
        && (chosen.len() != own.len() || chosen.iter().any(|id| !own.contains(&id.as_str())))
}

fn publish_draft(ui: &AppWindow) {
    let (event_id, ready) = STATE.with_borrow(|state| match &state.draft {
        Some(draft) => (draft.event_id.clone(), draft.ready),
        None => (String::new(), false),
    });
    let global = ui.global::<PollUiState>();
    global.set_draft_event_id(event_id.into());
    global.set_draft_ready(ready);
    global.set_draft_revision(global.get_draft_revision().wrapping_add(1));
}

fn set_notice(ui: &AppWindow, event_id: &str, text: &str, error: bool) {
    let global = ui.global::<PollUiState>();
    global.set_notice_event_id(event_id.into());
    global.set_notice(text.into());
    global.set_notice_error(error);
}

fn choose_option(ui: &AppWindow, event_id: &str, option_id: &str, thread_root: &str) {
    if scope(ui).is_none() {
        return;
    }
    let Some(info) = shown_poll(ui, event_id, thread_root) else {
        return;
    };
    let Some(poll) = info.poll.as_ref() else {
        return;
    };
    if poll.ended
        || poll.max_selections == 0
        || !poll.answers.iter().any(|answer| answer.id == option_id)
    {
        return;
    }
    let limit = usize::try_from(poll.max_selections).unwrap_or(1);
    let own = poll.own_selection();
    let accepted = STATE.with_borrow_mut(|state| {
        if state.pending.is_some() {
            return None;
        }
        let draft = state.draft.get_or_insert_with(|| Draft {
            event_id: String::new(),
            chosen: Vec::new(),
            ready: false,
        });
        if draft.event_id != event_id {
            draft.event_id = event_id.to_owned();
            draft.chosen = own.iter().map(|id| (*id).to_owned()).collect();
        }
        // An edit of the poll may have removed answers or lowered its limit.
        draft
            .chosen
            .retain(|id| poll.answers.iter().any(|answer| answer.id == *id));
        draft.chosen.truncate(limit);
        let accepted = toggle(&mut draft.chosen, option_id, limit);
        draft.ready = differs(&draft.chosen, &own);
        Some(accepted)
    });
    let Some(accepted) = accepted else {
        return;
    };
    let global = ui.global::<PollUiState>();
    global.set_confirm_end_event_id("".into());
    if accepted {
        if global.get_notice_event_id().as_str() == event_id {
            set_notice(ui, "", "", false);
        }
    } else {
        let text = format!("You can choose up to {limit} options. Clear one first.");
        set_notice(ui, event_id, &text, true);
    }
    publish_draft(ui);
}

/// Mark one action in flight for `event_id` and return its operation.
fn begin(ui: &AppWindow, event_id: &str, text: &str) -> u64 {
    let operation = STATE.with_borrow_mut(|state| {
        state.operation = state.operation.wrapping_add(1);
        state.pending = Some(state.operation);
        state.operation
    });
    let global = ui.global::<PollUiState>();
    global.set_busy_event_id(event_id.into());
    set_notice(ui, event_id, text, false);
    operation
}

struct Outcome {
    operation: u64,
    selection: AccountSelection,
    account_id: String,
    room_id: String,
    event_id: String,
    action: Action,
    result: Result<(), PollError>,
}

fn finish(ui: &AppWindow, outcome: Outcome) {
    let current = STATE.with_borrow_mut(|state| {
        if state.pending != Some(outcome.operation) {
            return false;
        }
        state.pending = None;
        if outcome.result.is_ok()
            && outcome.action == Action::Vote
            && matches!(&state.draft, Some(draft) if draft.event_id == outcome.event_id)
        {
            state.draft = None;
        }
        true
    });
    if !current {
        return;
    }
    let global = ui.global::<PollUiState>();
    global.set_busy_event_id("".into());
    // A late result after the account or room changed is not shown there.
    if !outcome.selection.is_current() || !shows(ui, &outcome.account_id, &outcome.room_id) {
        set_notice(ui, "", "", false);
        return;
    }
    match (outcome.result, outcome.action) {
        (Ok(()), Action::Vote) => {
            publish_draft(ui);
            set_notice(ui, &outcome.event_id, VOTE_SENT, false);
        }
        (Ok(()), Action::End) => {
            global.set_confirm_end_event_id("".into());
            set_notice(ui, &outcome.event_id, END_SENT, false);
        }
        (Err(error), _) => set_notice(ui, &outcome.event_id, &error.to_string(), true),
    }
}

fn vote(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle, event_id: &str, thread_root: &str) {
    let Some((account_id, room_id)) = scope(ui) else {
        return;
    };
    let chosen = STATE.with_borrow(|state| {
        if state.pending.is_some() {
            return None;
        }
        state
            .draft
            .as_ref()
            .filter(|draft| draft.event_id == event_id && draft.ready)
            .map(|draft| draft.chosen.clone())
    });
    let Some(chosen) = chosen else {
        return;
    };
    let Some(info) = shown_poll(ui, event_id, thread_root) else {
        set_notice(ui, event_id, &PollError::Unavailable.to_string(), true);
        return;
    };
    let Some(poll) = info.poll.as_ref() else {
        return;
    };
    // The core checks again against the live timeline item before sending.
    let limit = usize::try_from(poll.max_selections).unwrap_or(0);
    let known = chosen
        .iter()
        .all(|id| poll.answers.iter().any(|answer| &answer.id == id));
    if poll.ended || !known || chosen.len() > limit {
        // Fall back to the counted vote instead of keeping an unsendable draft.
        STATE.with_borrow_mut(|state| state.draft = None);
        publish_draft(ui);
        set_notice(ui, event_id, &PollError::InvalidChoice.to_string(), true);
        return;
    }
    let Some(selection) = live.manager.account_selection(&account_id) else {
        set_notice(ui, event_id, ACCOUNT_CHANGED, true);
        return;
    };
    let operation = begin(ui, event_id, VOTE_SENDING);
    let thread = (!thread_root.is_empty()).then(|| thread_root.to_owned());
    let focused = thread
        .is_none()
        .then(|| live.focused_view(&room_id))
        .flatten();
    let event_id = event_id.to_owned();
    let live = live.clone();
    let weak = ui.as_weak();
    handle.spawn(async move {
        let result = live
            .manager
            .vote_poll_for(
                &selection,
                &room_id,
                thread.as_deref(),
                focused.as_ref(),
                &event_id,
                &chosen,
            )
            .await;
        let outcome = Outcome {
            operation,
            selection,
            account_id,
            room_id,
            event_id,
            action: Action::Vote,
            result,
        };
        let _ = weak.upgrade_in_event_loop(move |ui| finish(&ui, outcome));
    });
}

/// Only an own, sent, open poll offers End; the card confirms first.
fn endable(ui: &AppWindow, event_id: &str, thread_root: &str) -> bool {
    shown_poll(ui, event_id, thread_root)
        .is_some_and(|info| info.is_own && info.poll.as_ref().is_some_and(|poll| !poll.ended))
}

fn request_end(ui: &AppWindow, event_id: &str) {
    let thread_root = ui.get_thread_root_id();
    let shown = endable(ui, event_id, "") || endable(ui, event_id, thread_root.as_str());
    if scope(ui).is_none() || !idle() || !shown {
        return;
    }
    let global = ui.global::<PollUiState>();
    global.set_confirm_end_event_id(event_id.into());
}

fn confirm_end(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    handle: &Handle,
    event_id: &str,
    thread_root: &str,
) {
    let global = ui.global::<PollUiState>();
    let Some((account_id, room_id)) = scope(ui) else {
        return;
    };
    if !idle() || global.get_confirm_end_event_id().as_str() != event_id {
        return;
    }
    if !endable(ui, event_id, thread_root) {
        global.set_confirm_end_event_id("".into());
        set_notice(ui, event_id, &PollError::Unavailable.to_string(), true);
        return;
    }
    let Some(selection) = live.manager.account_selection(&account_id) else {
        set_notice(ui, event_id, ACCOUNT_CHANGED, true);
        return;
    };
    let operation = begin(ui, event_id, END_SENDING);
    let thread = (!thread_root.is_empty()).then(|| thread_root.to_owned());
    let focused = thread
        .is_none()
        .then(|| live.focused_view(&room_id))
        .flatten();
    let event_id = event_id.to_owned();
    let live = live.clone();
    let weak = ui.as_weak();
    handle.spawn(async move {
        let result = live
            .manager
            .end_poll_for(
                &selection,
                &room_id,
                thread.as_deref(),
                focused.as_ref(),
                &event_id,
            )
            .await;
        let outcome = Outcome {
            operation,
            selection,
            account_id,
            room_id,
            event_id,
            action: Action::End,
            result,
        };
        let _ = weak.upgrade_in_event_loop(move |ui| finish(&ui, outcome));
    });
}

/// Mirror the dialog draft's state; validation guidance waits until the
/// person has typed something.
fn publish_dialog(ui: &AppWindow) {
    let snapshot = STATE.with_borrow(|state| {
        let dialog = state.dialog.as_ref()?;
        let draft = &dialog.draft;
        let typed = !draft.question.trim().is_empty()
            || draft.answers.iter().any(|answer| !answer.trim().is_empty());
        let error = draft.validate().err();
        Some((
            draft.max_selections,
            draft.disclosed,
            dialog.busy,
            typed,
            error,
        ))
    });
    let Some((max, disclosed, busy, typed, error)) = snapshot else {
        return;
    };
    let global = ui.global::<PollUiState>();
    global.set_dialog_max(i32::try_from(max).unwrap_or(1));
    global.set_dialog_disclosed(disclosed);
    global.set_dialog_busy(busy);
    global.set_dialog_ready(error.is_none() && !busy);
    let text = error
        .filter(|_| typed)
        .map(|error| error.to_string())
        .unwrap_or_default();
    global.set_dialog_error(text.into());
}

fn edit_dialog(ui: &AppWindow, edit: impl FnOnce(&mut PollDraft)) {
    let edited = STATE.with_borrow_mut(|state| match state.dialog.as_mut() {
        Some(dialog) if !dialog.busy => {
            let previous = dialog.draft.clone();
            edit(&mut dialog.draft);
            let count = dialog.draft.answers.len().max(1);
            dialog.draft.max_selections = dialog.draft.max_selections.clamp(1, count);
            if dialog.draft == previous {
                // Re-entering identical text or a clamped step is still the
                // same poll. Preserve its retry transaction after uncertainty.
                return false;
            }
            dialog.key = PollSendKey::new();
            true
        }
        _ => false,
    });
    if edited {
        publish_dialog(ui);
    }
}

fn open_dialog(ui: &AppWindow, live: &Arc<LiveUi>) {
    if ui.get_poll_dialog_open() || ui.get_composer_mode() != 0 {
        return;
    }
    let Some((account_id, room_id)) = scope(ui) else {
        return;
    };
    let Some(selection) = live.manager.account_selection(&account_id) else {
        return;
    };
    let draft = PollDraft {
        question: String::new(),
        answers: vec![String::new(); MIN_POLL_ANSWER_COUNT],
        max_selections: 1,
        disclosed: true,
    };
    STATE.with_borrow_mut(|state| {
        state.operation = state.operation.wrapping_add(1);
        state.dialog = Some(Dialog {
            operation: state.operation,
            selection,
            account_id,
            room_id,
            draft,
            key: PollSendKey::new(),
            busy: false,
        });
    });
    let room = crate::directory_plain_text(ui.get_active_room_name().as_str(), 80);
    let room = if room.is_empty() {
        "this room".to_owned()
    } else {
        room
    };
    let global = ui.global::<PollUiState>();
    global.set_dialog_room(room.into());
    global.set_dialog_question("".into());
    let rows = vec![SharedString::new(); MIN_POLL_ANSWER_COUNT];
    global.set_dialog_answers(ModelRc::new(VecModel::from(rows)));
    publish_dialog(ui);
    crate::matrix_links::clear(ui);
    ui.set_poll_dialog_open(true);
}

fn close_dialog(ui: &AppWindow) {
    STATE.with_borrow_mut(|state| state.dialog = None);
    let global = ui.global::<PollUiState>();
    global.set_dialog_question("".into());
    global.set_dialog_answers(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
    global.set_dialog_busy(false);
    global.set_dialog_ready(false);
    global.set_dialog_error("".into());
    ui.set_poll_dialog_open(false);
}

fn submit_dialog(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {
    let job = STATE.with_borrow_mut(|state| {
        let dialog = state.dialog.as_mut()?;
        let same = shows(ui, &dialog.account_id, &dialog.room_id);
        if dialog.busy || !same || dialog.draft.validate().is_err() {
            return None;
        }
        dialog.busy = true;
        Some((
            dialog.operation,
            dialog.selection.clone(),
            dialog.account_id.clone(),
            dialog.room_id.clone(),
            dialog.draft.clone(),
            dialog.key.clone(),
        ))
    });
    let Some((operation, selection, account_id, room_id, draft, key)) = job else {
        return;
    };
    publish_dialog(ui);
    let live = live.clone();
    let weak = ui.as_weak();
    handle.spawn(async move {
        let result = live
            .manager
            .create_poll_for(&selection, &room_id, &draft, &key)
            .await;
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let current = STATE.with_borrow_mut(|state| match state.dialog.as_mut() {
                Some(dialog) if dialog.operation == operation && dialog.busy => {
                    dialog.busy = false;
                    true
                }
                _ => false,
            });
            if !current {
                return;
            }
            if !selection.is_current() || !shows(&ui, &account_id, &room_id) {
                // Never leave a dialog whose busy state can no longer clear.
                close_dialog(&ui);
                return;
            }
            match result {
                Ok(()) => {
                    close_dialog(&ui);
                    ui.invoke_focus_composer(false);
                }
                Err(error) => {
                    publish_dialog(&ui);
                    let global = ui.global::<PollUiState>();
                    global.set_dialog_error(error.to_string().into());
                }
            }
        });
    });
}

/// Forget every poll draft, pending action, notice and the dialog. Called on
/// every account, room or thread change, logout and detach.
pub(crate) fn clear_presentation(ui: &AppWindow) {
    STATE.with_borrow_mut(|state| {
        state.operation = state.operation.wrapping_add(1);
        state.draft = None;
        state.pending = None;
    });
    let global = ui.global::<PollUiState>();
    global.set_draft_event_id("".into());
    global.set_draft_ready(false);
    global.set_draft_revision(global.get_draft_revision().wrapping_add(1));
    global.set_busy_event_id("".into());
    global.set_confirm_end_event_id("".into());
    set_notice(ui, "", "", false);
    close_dialog(ui);
}

/// The dialog's option rows, edited in place so other fields keep focus.
fn answer_rows(ui: &AppWindow, edit: impl FnOnce(&VecModel<SharedString>)) {
    let model = ui.global::<PollUiState>().get_dialog_answers();
    if let Some(rows) = model.as_any().downcast_ref::<VecModel<SharedString>>() {
        edit(rows);
    }
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, handle: Handle) {
    let global = ui.global::<PollUiState>();
    global.on_option_chosen(|event_id, option_id, _revision| {
        STATE.with_borrow(|state| {
            state.draft.as_ref().is_some_and(|draft| {
                draft.event_id == event_id.as_str()
                    && draft.chosen.iter().any(|id| id == option_id.as_str())
            })
        })
    });

    let weak = ui.as_weak();
    global.on_choose_option(move |event_id, option_id, thread_root| {
        if let Some(ui) = weak.upgrade() {
            choose_option(&ui, &event_id, &option_id, &thread_root);
        }
    });

    let weak = ui.as_weak();
    let vote_live = live.clone();
    let vote_handle = handle.clone();
    global.on_vote(move |event_id, thread_root| {
        if let Some(ui) = weak.upgrade() {
            vote(&ui, &vote_live, &vote_handle, &event_id, &thread_root);
        }
    });

    let weak = ui.as_weak();
    global.on_request_end(move |event_id| {
        if let Some(ui) = weak.upgrade() {
            request_end(&ui, &event_id);
        }
    });

    let weak = ui.as_weak();
    let end_live = live.clone();
    let end_handle = handle.clone();
    global.on_confirm_end(move |event_id, thread_root| {
        if let Some(ui) = weak.upgrade() {
            confirm_end(&ui, &end_live, &end_handle, &event_id, &thread_root);
        }
    });

    let weak = ui.as_weak();
    global.on_cancel_end(move || {
        if let Some(ui) = weak.upgrade() {
            let global = ui.global::<PollUiState>();
            global.set_confirm_end_event_id("".into());
        }
    });

    let weak = ui.as_weak();
    global.on_question_edited(move |text| {
        if let Some(ui) = weak.upgrade() {
            edit_dialog(&ui, |draft| draft.question = text.to_string());
        }
    });

    let weak = ui.as_weak();
    global.on_answer_edited(move |index, text| {
        let Some(ui) = weak.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let mut edited = false;
        edit_dialog(&ui, |draft| {
            if let Some(answer) = draft.answers.get_mut(index) {
                *answer = text.to_string();
                edited = true;
            }
        });
        if edited {
            answer_rows(&ui, |rows| rows.set_row_data(index, text.clone()));
        }
    });

    let weak = ui.as_weak();
    global.on_add_answer(move || {
        let Some(ui) = weak.upgrade() else { return };
        let mut added = false;
        edit_dialog(&ui, |draft| {
            if draft.answers.len() < MAX_POLL_ANSWER_COUNT {
                draft.answers.push(String::new());
                added = true;
            }
        });
        if added {
            answer_rows(&ui, |rows| rows.push(SharedString::new()));
        }
    });

    let weak = ui.as_weak();
    global.on_remove_answer(move |index| {
        let Some(ui) = weak.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let mut removed = false;
        edit_dialog(&ui, |draft| {
            if draft.answers.len() > MIN_POLL_ANSWER_COUNT && index < draft.answers.len() {
                draft.answers.remove(index);
                removed = true;
            }
        });
        if removed {
            answer_rows(&ui, |rows| {
                rows.remove(index);
            });
        }
    });

    let weak = ui.as_weak();
    global.on_change_max(move |delta| {
        if let Some(ui) = weak.upgrade() {
            edit_dialog(&ui, |draft| {
                let next = i64::try_from(draft.max_selections).unwrap_or(1) + i64::from(delta);
                draft.max_selections = usize::try_from(next.max(1)).unwrap_or(1);
            });
        }
    });

    let weak = ui.as_weak();
    global.on_set_disclosed(move |disclosed| {
        if let Some(ui) = weak.upgrade() {
            edit_dialog(&ui, |draft| draft.disclosed = disclosed);
        }
    });

    let weak = ui.as_weak();
    let submit_live = live.clone();
    global.on_submit_poll(move || {
        if let Some(ui) = weak.upgrade() {
            submit_dialog(&ui, &submit_live, &handle);
        }
    });

    let weak = ui.as_weak();
    global.on_cancel_poll(move || {
        if let Some(ui) = weak.upgrade()
            && !dialog_busy()
        {
            close_dialog(&ui);
            // The closed dialog took keyboard focus with it.
            ui.invoke_focus_composer(false);
        }
    });

    let weak = ui.as_weak();
    ui.on_open_create_poll(move || {
        if let Some(ui) = weak.upgrade() {
            open_dialog(&ui, &live);
        }
    });

    let weak = ui.as_weak();
    ui.on_poll_context_changed(move || {
        if let Some(ui) = weak.upgrade() {
            clear_presentation(&ui);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn single_choice_replaces_and_multiple_choice_respects_the_limit() {
        let mut chosen = ids(&["a"]);
        assert!(toggle(&mut chosen, "b", 1));
        assert_eq!(chosen, ["b"]);

        let mut chosen = ids(&["a", "b"]);
        assert!(!toggle(&mut chosen, "c", 2));
        assert_eq!(chosen, ["a", "b"]);
        assert!(toggle(&mut chosen, "a", 2));
        assert!(toggle(&mut chosen, "c", 2));
        assert_eq!(chosen, ["b", "c"]);
    }

    #[test]
    fn only_a_changed_non_empty_choice_can_be_sent() {
        assert!(!differs(&ids(&[]), &[]));
        assert!(!differs(&ids(&["a"]), &["a"]));
        assert!(!differs(&ids(&["b", "a"]), &["a", "b"]));
        assert!(differs(&ids(&["a"]), &[]));
        assert!(differs(&ids(&["a"]), &["a", "b"]));
        assert!(differs(&ids(&["c"]), &["a"]));
    }
}
