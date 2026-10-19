//! Identity-change warnings for the open room. One task follows the visible
//! room's warning feed under the account selection captured for that view;
//! any account, room or live-session change replaces it, and every update is
//! applied only while that same view is shown. Trust changes only through
//! the banner's explicit Accept or Withdraw action.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use morpheus_matrix::{IdentityWarningKind, IdentityWarnings};
use slint::ComponentHandle;
use tokio::{runtime::Handle, task::AbortHandle};

use crate::{AppWindow, live::LiveUi};

static WATCHER: Mutex<Option<AbortHandle>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn kind_code(kind: IdentityWarningKind) -> i32 {
    match kind {
        IdentityWarningKind::Changed => 1,
        IdentityWarningKind::VerificationLost => 2,
    }
}

fn code_kind(code: i32) -> Option<IdentityWarningKind> {
    match code {
        1 => Some(IdentityWarningKind::Changed),
        2 => Some(IdentityWarningKind::VerificationLost),
        _ => None,
    }
}

fn scope(ui: &AppWindow) -> Option<(String, String)> {
    let account = ui.get_active_account_local_id();
    let room = ui.get_active_room_id();
    (ui.get_live_session() && !account.is_empty() && !room.is_empty())
        .then(|| (account.to_string(), room.to_string()))
}

fn stop_watcher() {
    if let Some(task) = WATCHER.lock().unwrap_or_else(|error| error.into_inner()).take() {
        task.abort();
    }
}

fn blank(ui: &AppWindow) {
    ui.set_identity_warning_user("".into());
    ui.set_identity_warning_name("".into());
    ui.set_identity_warning_kind(0);
    ui.set_identity_warning_others(0);
    ui.set_identity_warning_busy(false);
    ui.set_identity_warning_error("".into());
}

fn show(ui: &AppWindow, warnings: &IdentityWarnings) {
    let Some(first) = warnings.warnings.first() else {
        blank(ui);
        return;
    };
    if ui.get_identity_warning_user().as_str() != first.user_id {
        // A different member's warning: an old error or pending state does
        // not belong to it.
        ui.set_identity_warning_busy(false);
        ui.set_identity_warning_error("".into());
    }
    let others = warnings.warnings.len() - 1 + warnings.more;
    let name = first.display_name.as_deref().unwrap_or(&first.user_id);
    ui.set_identity_warning_user(first.user_id.as_str().into());
    ui.set_identity_warning_name(name.into());
    ui.set_identity_warning_kind(kind_code(first.kind));
    ui.set_identity_warning_others(i32::try_from(others).unwrap_or(i32::MAX));
}

/// Stop following and forget the shown warning. Called on every account
/// change, logout and detach; the scope change starts the next feed.
pub(crate) fn clear_ui(ui: &AppWindow) {
    stop_watcher();
    GENERATION.fetch_add(1, Ordering::AcqRel);
    blank(ui);
}

fn follow(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {
    clear_ui(ui);
    let Some((account_id, room_id)) = scope(ui) else {
        return;
    };
    let Some(selection) = live.manager.account_selection(&account_id) else {
        return;
    };
    let generation = GENERATION.load(Ordering::Acquire);
    let manager = live.manager.clone();
    let weak = ui.as_weak();
    let task = handle.spawn(async move {
        // Rooms without encryption state or identities simply show nothing.
        let Ok(feed) = manager.identity_warnings_for(&selection, &room_id).await else {
            return;
        };
        let mut updates = feed.subscribe();
        loop {
            let warnings = updates.borrow_and_update().clone();
            let shown = (account_id.clone(), room_id.clone());
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if GENERATION.load(Ordering::Acquire) == generation && scope(&ui) == Some(shown) {
                    show(&ui, &warnings);
                }
            });
            let changed = selection.guard(updates.changed()).await;
            if !matches!(changed, Some(Ok(()))) {
                break;
            }
        }
        drop(feed);
    });
    *WATCHER.lock().unwrap_or_else(|error| error.into_inner()) = Some(task.abort_handle());
}

fn acknowledge(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {
    if ui.get_identity_warning_busy() {
        return;
    }
    let Some((account_id, room_id)) = scope(ui) else {
        return;
    };
    let user_id = ui.get_identity_warning_user().to_string();
    let Some(kind) = code_kind(ui.get_identity_warning_kind()) else {
        return;
    };
    if user_id.is_empty() {
        return;
    }
    let Some(selection) = live.manager.account_selection(&account_id) else {
        return;
    };
    let generation = GENERATION.load(Ordering::Acquire);
    ui.set_identity_warning_busy(true);
    ui.set_identity_warning_error("".into());
    let manager = live.manager.clone();
    let weak = ui.as_weak();
    handle.spawn(async move {
        let result = manager
            .acknowledge_identity_for(&selection, &room_id, &user_id, kind)
            .await;
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let same = GENERATION.load(Ordering::Acquire) == generation
                && scope(&ui) == Some((account_id, room_id))
                && ui.get_identity_warning_user().as_str() == user_id;
            if !same {
                return;
            }
            ui.set_identity_warning_busy(false);
            // On success the feed retires the warning once the SDK reports
            // the new state; nothing is hidden before that.
            if let Err(error) = result {
                ui.set_identity_warning_error(error.to_string().into());
            }
        });
    });
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, handle: Handle) {
    let weak = ui.as_weak();
    let follow_live = live.clone();
    let follow_handle = handle.clone();
    ui.on_identity_scope_changed(move || {
        if let Some(ui) = weak.upgrade() {
            follow(&ui, &follow_live, &follow_handle);
        }
    });

    let weak = ui.as_weak();
    ui.on_acknowledge_identity_warning(move || {
        if let Some(ui) = weak.upgrade() {
            acknowledge(&ui, &live, &handle);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_kinds_round_trip_and_unknown_codes_act_on_nothing() {
        for kind in [
            IdentityWarningKind::Changed,
            IdentityWarningKind::VerificationLost,
        ] {
            assert_eq!(code_kind(kind_code(kind)), Some(kind));
        }
        assert_eq!(code_kind(0), None);
        assert_eq!(code_kind(3), None);
    }
}
