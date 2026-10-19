//! Verification requests other people send this account. One task follows
//! the live account's requests under the selection captured for it and shows
//! only the newest pending one; requests that end, expire or are cancelled
//! retire. Nothing is accepted until the person chooses Review; Decline
//! cancels the request for both people, and Not now only hides it here.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use morpheus_matrix::PersonVerificationRequest;
use slint::ComponentHandle;
use tokio::{runtime::Handle, task::AbortHandle};

use crate::{AppWindow, live::LiveUi};

/// Re-check the shown request this often so an expired one retires.
const RECHECK: Duration = Duration::from_secs(30);

static WATCHER: Mutex<Option<AbortHandle>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn scope(ui: &AppWindow) -> Option<String> {
    let account = ui.get_active_account_local_id();
    (ui.get_live_session() && !account.is_empty()).then(|| account.to_string())
}

fn stop_watcher() {
    let previous = WATCHER
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    if let Some(task) = previous {
        task.abort();
    }
}

fn show(ui: &AppWindow, request: Option<&PersonVerificationRequest>) {
    let (user, name, flow) = match request {
        Some(request) => (
            request.user_id.as_str(),
            request.display_name.as_deref().unwrap_or(&request.user_id),
            request.flow_id.as_str(),
        ),
        None => ("", "", ""),
    };
    if ui.get_person_request_flow().as_str() != flow {
        ui.set_person_request_busy(false);
        ui.set_person_request_error("".into());
    }
    ui.set_person_request_user(user.into());
    ui.set_person_request_name(name.into());
    ui.set_person_request_flow(flow.into());
}

/// Stop following and forget the shown request. Called on every account
/// change, logout and detach; the scope change starts the next watch.
pub(crate) fn clear_ui(ui: &AppWindow) {
    stop_watcher();
    GENERATION.fetch_add(1, Ordering::AcqRel);
    show(ui, None);
    ui.set_person_request_busy(false);
    ui.set_person_request_error("".into());
    ui.set_person_request_dismissed("".into());
}

fn follow(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {
    clear_ui(ui);
    let Some(account_id) = scope(ui) else {
        return;
    };
    let Some(selection) = live.manager.account_selection(&account_id) else {
        return;
    };
    let generation = GENERATION.load(Ordering::Acquire);
    let manager = live.manager.clone();
    let weak = ui.as_weak();
    let task = handle.spawn(async move {
        let Ok(watch) = manager
            .watch_person_verification_requests_for(&account_id)
            .await
        else {
            return;
        };
        let mut updates = watch.subscribe();
        loop {
            let newest = updates.borrow_and_update().last().cloned();
            if let Some(request) = &newest {
                let pending = manager
                    .verification_outcome_for(&account_id, &request.user_id, &request.flow_id)
                    .await
                    .is_ok_and(|outcome| !outcome.info.is_done && !outcome.info.is_cancelled);
                if !pending {
                    watch.retire(&request.flow_id);
                    continue;
                }
            }
            let shown = account_id.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if GENERATION.load(Ordering::Acquire) == generation
                    && scope(&ui).as_deref() == Some(shown.as_str())
                {
                    show(&ui, newest.as_ref());
                }
            });
            // A new request wakes this at once; otherwise re-check later so
            // an expired or cancelled request does not stay on screen.
            let next = selection
                .guard(tokio::time::timeout(RECHECK, updates.changed()))
                .await;
            if matches!(next, None | Some(Ok(Err(_)))) {
                break;
            }
        }
        drop(watch);
    });
    *WATCHER.lock().unwrap_or_else(|error| error.into_inner()) = Some(task.abort_handle());
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, handle: Handle) {
    let weak = ui.as_weak();
    let follow_live = live.clone();
    let follow_handle = handle.clone();
    ui.on_person_request_scope_changed(move || {
        if let Some(ui) = weak.upgrade() {
            follow(&ui, &follow_live, &follow_handle);
        }
    });

    let weak = ui.as_weak();
    let decline_live = live.clone();
    let decline_handle = handle.clone();
    ui.on_decline_person_request(move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some(account_id) = scope(&ui) else { return };
        let user_id = ui.get_person_request_user().to_string();
        let flow_id = ui.get_person_request_flow().to_string();
        if user_id.is_empty() || flow_id.is_empty() || ui.get_person_request_busy() {
            return;
        }
        ui.set_person_request_busy(true);
        ui.set_person_request_error("".into());
        let generation = GENERATION.load(Ordering::Acquire);
        let live = decline_live.clone();
        let weak = ui.as_weak();
        decline_handle.spawn(async move {
            let result = {
                let _transition = live.account_transition.lock().await;
                live.manager
                    .decline_verification_for(&account_id, &user_id, &flow_id)
                    .await
            };
            let _ = weak.upgrade_in_event_loop(move |ui| {
                // Only the request that was declined, in the same account view.
                if GENERATION.load(Ordering::Acquire) != generation
                    || scope(&ui).as_deref() != Some(account_id.as_str())
                    || ui.get_person_request_flow().as_str() != flow_id
                {
                    return;
                }
                ui.set_person_request_busy(false);
                match result {
                    Ok(()) => ui.set_person_request_dismissed(flow_id.into()),
                    Err(_) => ui.set_person_request_error(
                        "Could not decline. The request may have expired.".into(),
                    ),
                }
            });
        });
    });

    let weak = ui.as_weak();
    ui.on_review_person_request(move || {
        let Some(ui) = weak.upgrade() else { return };
        if scope(&ui).is_none() {
            return;
        }
        // Accept exactly the request the banner shows.
        let user_id = ui.get_person_request_user().to_string();
        let flow_id = ui.get_person_request_flow().to_string();
        let name = ui.get_person_request_name().to_string();
        if !user_id.is_empty() && !flow_id.is_empty() {
            crate::verification::accept_person(&ui, live.clone(), &handle, user_id, flow_id, name);
        }
    });
}
