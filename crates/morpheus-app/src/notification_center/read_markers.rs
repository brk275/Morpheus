//! Withdrawal of banners for rooms that were read.
//!
//! Read receipts for the conversation on screen belong to
//! `crate::unread_navigation`: a private receipt is sent only for messages
//! whose rows were actually visible in the active window, and the room's
//! banners are withdrawn once its latest message was seen. This task only
//! withdraws banners of the active account's rooms whose unread count
//! reached zero, because another device or client read them, after each
//! sync batch. It never sends a receipt. Nothing runs without a live
//! account, and requests are fenced by the account's selection.

use std::sync::Arc;

use matrix_sdk::sync::RoomUpdates;
use tokio::sync::broadcast;

use crate::{live::LiveUi, notifications::DesktopNotifications};

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

pub(super) async fn run(live: Arc<LiveUi>, notifications: Arc<DesktopNotifications>) {
    let manager = live.manager.clone();
    let mut active = manager.subscribe_active_account();
    loop {
        // Room updates belong to one account's client; subscribe again
        // after every account change.
        let mut updates = manager
            .subscribe_room_updates()
            .await
            .ok()
            .map(|(_, receiver)| receiver);
        loop {
            let batch = tokio::select! {
                changed = active.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    break;
                }
                batch = next_batch(&mut updates) => batch,
            };
            if notifications.is_stopped() {
                return;
            }
            if batch.is_none() {
                updates = None;
                continue;
            }
            withdraw_read_elsewhere(&live, &notifications).await;
        }
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
