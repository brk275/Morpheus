//! Older replies of the open thread. A thread panel shows the replies its
//! feed has loaded; this pages that same thread timeline backwards under the
//! account selection captured at the click, and publishes only to the same
//! account, room and thread.

use std::sync::Arc;
use std::time::Duration;

use slint::ComponentHandle;
use tokio::runtime::Handle;

use crate::{AppWindow, live::LiveUi};

/// Replies requested per click.
const PAGE_EVENTS: u16 = 50;
const TIMEOUT: Duration = Duration::from_secs(30);
const FAILED: &str = "Could not load earlier replies. Check sync and retry.";

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: Handle) {
    let weak = ui.as_weak();
    ui.on_load_older_thread_replies(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !window.get_live_session()
            || window.get_thread_history_loading()
            || window.get_thread_history_at_start()
        {
            return;
        }
        let account = window.get_active_account_local_id().to_string();
        let room = window.get_active_room_id().to_string();
        let root = window.get_thread_root_id().to_string();
        if account.is_empty() || room.is_empty() || root.is_empty() {
            return;
        }
        // Bind the click before spawning, as room history does.
        let Some(selection) = live.manager.account_selection(&account) else {
            return;
        };
        let operation = window.get_thread_history_operation().wrapping_add(1);
        window.set_thread_history_operation(operation);
        window.set_thread_history_loading(true);
        window.set_thread_history_error("".into());
        let manager = live.manager.clone();
        let weak = weak.clone();
        runtime.spawn(async move {
            let page =
                manager.paginate_thread_backwards_selected(&selection, &room, &root, PAGE_EVENTS);
            let result = tokio::time::timeout(TIMEOUT, page).await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                if window.get_active_account_local_id().as_str() != account
                    || window.get_active_room_id().as_str() != room
                    || window.get_thread_root_id().as_str() != root
                    || window.get_thread_history_operation() != operation
                {
                    return;
                }
                // The same thread is shown: end loading even when the
                // account selection ended (A -> B -> A) and drop its result.
                window.set_thread_history_loading(false);
                if !selection.is_current() {
                    return;
                }
                match result {
                    Ok(Ok(start_reached)) => window.set_thread_history_at_start(start_reached),
                    Ok(Err(_)) | Err(_) => window.set_thread_history_error(FAILED.into()),
                }
            });
        });
    });
}
