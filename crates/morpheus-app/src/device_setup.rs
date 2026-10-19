//! App-wide prompt to finish securing this device: verify it, or enter the
//! recovery key where recovery is incomplete. One task follows the live
//! account's SDK state under the selection captured for it. The prompt only
//! opens Security settings; it never verifies, recovers or resets anything.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use futures_util::StreamExt;
use morpheus_matrix::DeviceSetupNeed;
use slint::ComponentHandle;
use tokio::{runtime::Handle, task::AbortHandle};

use crate::{AppWindow, live::LiveUi};

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

/// The window's value for each need: 0 nothing, 1 verify, 2 recovery key.
fn need_code(need: DeviceSetupNeed) -> i32 {
    match need {
        DeviceSetupNeed::None => 0,
        DeviceSetupNeed::Verify => 1,
        DeviceSetupNeed::EnterRecoveryKey => 2,
    }
}

/// Stop following and hide the prompt. Called on every account change,
/// logout and detach; the scope change starts the next watch.
pub(crate) fn clear_ui(ui: &AppWindow) {
    stop_watcher();
    GENERATION.fetch_add(1, Ordering::AcqRel);
    ui.set_device_setup_need(0);
    ui.set_device_setup_dismissed(false);
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
        let Ok(mut needs) = manager.watch_device_setup_for(&account_id).await else {
            return;
        };
        while let Some(Some(need)) = selection.guard(needs.next()).await {
            let shown = account_id.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if GENERATION.load(Ordering::Acquire) == generation
                    && scope(&ui).as_deref() == Some(shown.as_str())
                {
                    ui.set_device_setup_need(need_code(need));
                }
            });
        }
    });
    *WATCHER.lock().unwrap_or_else(|error| error.into_inner()) = Some(task.abort_handle());
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, handle: Handle) {
    let weak = ui.as_weak();
    ui.on_device_setup_scope_changed(move || {
        if let Some(ui) = weak.upgrade() {
            follow(&ui, &live, &handle);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_setup_need_has_its_own_window_value() {
        assert_eq!(need_code(DeviceSetupNeed::None), 0);
        assert_eq!(need_code(DeviceSetupNeed::Verify), 1);
        assert_eq!(need_code(DeviceSetupNeed::EnterRecoveryKey), 2);
    }
}
