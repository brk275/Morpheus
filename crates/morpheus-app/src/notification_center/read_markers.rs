//! Read state for the room on screen, and withdrawal of read rooms' banners.
//!
//! A room counts as seen under the same rule that suppresses its banners:
//! the window is shown and not minimized, the conversation page is open and
//! the room is selected. Its live timeline (and an open thread's) is then
//! marked read with a private read receipt after each sync batch and when it
//! comes into view, so unread and mention badges clear here and on the
//! account's other devices. Banners of the seen room, and of any room whose
//! unread count reached zero elsewhere, are withdrawn where the platform can.
//! Nothing runs without a live account, and requests are fenced by the
//! account's selection.

use std::{sync::Arc, time::Duration};

use matrix_sdk::sync::RoomUpdates;
use slint::{ComponentHandle, Weak};
use tokio::{
    sync::{broadcast, oneshot},
    time::MissedTickBehavior,
};

use crate::{AppWindow, live::LiveUi, notifications::DesktopNotifications};

/// Visibility is sampled this often in addition to every sync batch, so a
/// room shown by navigation or by restoring the window is read promptly.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const UI_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
struct SeenView {
    account_id: String,
    room_id: String,
    thread_root: Option<String>,
}

/// What is on screen now; `Err` once the window's event loop has ended. A
/// UI that does not answer in time counts as showing nothing.
async fn seen_view(ui: &Weak<AppWindow>) -> Result<Option<SeenView>, ()> {
    let (send, receive) = oneshot::channel();
    ui.upgrade_in_event_loop(move |ui| {
        let account_id = ui.get_active_account_local_id().to_string();
        let room_id = ui.get_active_room_id().to_string();
        let shown = ui.get_live_session()
            && ui.get_page() == 0
            && ui.window().is_visible()
            && !ui.window().is_minimized()
            && !account_id.is_empty()
            && !room_id.is_empty();
        let thread_root = ui.get_thread_root_id().to_string();
        let thread_shown = ui.get_show_side_panel() && ui.get_side_panel_tab() == 1;
        let _ = send.send(shown.then(|| SeenView {
            account_id,
            room_id,
            thread_root: (thread_shown && !thread_root.is_empty()).then_some(thread_root),
        }));
    })
    .map_err(|_| ())?;
    Ok(tokio::time::timeout(UI_QUERY_TIMEOUT, receive)
        .await
        .ok()
        .and_then(Result::ok)
        .flatten())
}

/// One sync batch of the active account, or `None` once its feed ended.
async fn next_batch(updates: &mut Option<broadcast::Receiver<RoomUpdates>>) -> Option<()> {
    match updates {
        Some(receiver) => match receiver.recv().await {
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => Some(()),
            Err(broadcast::error::RecvError::Closed) => None,
        },
        None => std::future::pending().await,
    }
}

pub(super) async fn run(
    live: Arc<LiveUi>,
    notifications: Arc<DesktopNotifications>,
    ui: Weak<AppWindow>,
) {
    let manager = live.manager.clone();
    let mut active = manager.subscribe_active_account();
    let mut sample = tokio::time::interval(SAMPLE_INTERVAL);
    sample.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        // Room updates belong to one account's client; subscribe again
        // after every account change.
        let mut updates = manager
            .subscribe_room_updates()
            .await
            .ok()
            .map(|(_, receiver)| receiver);
        let mut last_seen: Option<SeenView> = None;
        let mut synced = true;
        loop {
            tokio::select! {
                changed = active.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    break;
                }
                batch = next_batch(&mut updates) => match batch {
                    Some(()) => synced = true,
                    None => updates = None,
                },
                _ = sample.tick() => {}
            }
            if notifications.is_stopped() {
                return;
            }
            let Ok(view) = seen_view(&ui).await else {
                return;
            };
            match view {
                Some(view) if synced || last_seen.as_ref() != Some(&view) => {
                    mark_seen(&live, &notifications, &view).await;
                    last_seen = Some(view);
                }
                Some(_) => {}
                None => last_seen = None,
            }
            if synced {
                withdraw_read_elsewhere(&live, &notifications).await;
            }
            synced = false;
        }
    }
}

/// Mark the seen room, and its open thread, read for the selection that
/// shows it, then withdraw the room's banners.
async fn mark_seen(live: &LiveUi, notifications: &DesktopNotifications, view: &SeenView) {
    let manager = &live.manager;
    let Some(selection) = manager.account_selection(&view.account_id) else {
        return;
    };
    let _ = manager
        .mark_room_read_selected(&selection, &view.room_id)
        .await;
    if let Some(root) = &view.thread_root {
        let _ = manager
            .mark_thread_read_selected(&selection, &view.room_id, root)
            .await;
    }
    if selection.is_current() {
        notifications.withdraw_room(&view.account_id, &view.room_id);
    }
}

/// Withdraw banners of the active account's rooms that another device or
/// client has read, whether or not the window is shown.
async fn withdraw_read_elsewhere(live: &LiveUi, notifications: &DesktopNotifications) {
    let manager = &live.manager;
    let Some(account) = manager.active_account().await else {
        return;
    };
    let rooms = notifications.withdrawable_rooms(&account.id);
    if rooms.is_empty() {
        return;
    }
    let Some(selection) = manager.account_selection(&account.id) else {
        return;
    };
    let Ok(counts) = manager.room_unread_counts_selected(&selection, &rooms).await else {
        return;
    };
    for counts in counts {
        if counts.notifications == 0 && selection.is_current() {
            notifications.withdraw_room(&account.id, &counts.room_id);
        }
    }
}
