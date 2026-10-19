//! Read state for the room on screen, and withdrawal of read rooms' banners.
//!
//! A room is on screen while the window is shown and not minimized, the
//! conversation page is open and the room is selected. Its live timeline
//! (and an open thread's) is marked read with a private read receipt when it
//! comes into view and when the window is activated again, so unread and
//! mention badges clear here and on the account's other devices. A sync
//! batch marks it read only while the window is active, or shortly after the
//! room came into view where the window system reports no activation, so a
//! window merely left open does not read messages nobody saw. Banners of the
//! read room, and of any room whose unread count reached zero elsewhere, are
//! withdrawn where the platform can. Nothing runs without a live account,
//! and requests are fenced by the account's selection.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

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
/// After a room comes into view, sync batches mark it read this long even
/// while the window reports no activation.
const IN_VIEW_GRACE: Duration = Duration::from_secs(2 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
struct SeenView {
    account_id: String,
    room_id: String,
    thread_root: Option<String>,
    /// The window is the active one.
    active: bool,
}

impl SeenView {
    fn same_place(&self, other: &Self) -> bool {
        (&self.account_id, &self.room_id, &self.thread_root)
            == (&other.account_id, &other.room_id, &other.thread_root)
    }
}

/// Whether the room on screen is marked read now: when it comes into view or
/// the window is activated again, and on a sync batch while the window is
/// active or within `IN_VIEW_GRACE` of the room coming into view.
fn should_mark(
    last: Option<&SeenView>,
    view: &SeenView,
    synced: bool,
    in_view_for: Duration,
) -> bool {
    let Some(last) = last.filter(|last| last.same_place(view)) else {
        return true;
    };
    (view.active && !last.active) || (synced && (view.active || in_view_for < IN_VIEW_GRACE))
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
            active: super::window_active(&ui),
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
        let mut in_view_since = Instant::now();
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
                Some(view) => {
                    let last = last_seen.take();
                    if !last.as_ref().is_some_and(|last| last.same_place(&view)) {
                        in_view_since = Instant::now();
                    }
                    if should_mark(last.as_ref(), &view, synced, in_view_since.elapsed()) {
                        mark_seen(&live, &notifications, &view).await;
                    }
                    last_seen = Some(view);
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn view(room: &str, active: bool) -> SeenView {
        SeenView {
            account_id: "alice".into(),
            room_id: room.into(),
            thread_root: None,
            active,
        }
    }

    #[test]
    fn a_window_merely_left_open_stops_reading_new_messages() {
        let shown = view("!a:example.org", false);
        let other = view("!b:example.org", true);
        let long = IN_VIEW_GRACE + Duration::from_secs(1);
        // Coming into view reads the room, whether the window is active.
        assert!(should_mark(None, &shown, false, Duration::ZERO));
        assert!(should_mark(Some(&other), &shown, false, long));
        // Sync reads it shortly after it came into view, then only while
        // the window is active.
        assert!(should_mark(Some(&shown), &shown, true, Duration::ZERO));
        assert!(!should_mark(Some(&shown), &shown, true, long));
        let active = view("!a:example.org", true);
        assert!(should_mark(Some(&active), &active, true, long));
        assert!(!should_mark(Some(&active), &active, false, long));
        // Activating the window again reads it.
        assert!(should_mark(Some(&shown), &active, false, long));
    }
}
