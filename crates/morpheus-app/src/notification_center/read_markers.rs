//! Withdrawal of banners for rooms that were read.
//!
//! After each sync batch of the active account, the banners Morpheus still
//! shows for its rooms are compared with the read state the SDK computed from
//! that account's receipts: a room whose unread notification count is zero,
//! because it was read here or on another device or client, loses its
//! banners where the platform can withdraw them. This module never marks a
//! room or thread read and sends no receipt of its own: the only automatic
//! receipt sender is the opt-in room activity reading path, and explicit
//! marking stays with its own action. Nothing runs without a live account,
//! requests are fenced by the account's selection, and inactive accounts are
//! never read.

use std::sync::Arc;

use matrix_sdk::sync::RoomUpdates;
use morpheus_matrix::RoomUnreadCounts;
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
            tokio::select! {
                changed = active.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    break;
                }
                batch = next_batch(&mut updates) => {
                    if batch.is_none() {
                        updates = None;
                        continue;
                    }
                }
            }
            if notifications.is_stopped() {
                return;
            }
            withdraw_read_elsewhere(&live, &notifications).await;
        }
    }
}

/// The rooms whose banners may go: those with no unread notification left,
/// whoever read them. A room that still has one keeps its banners.
fn read_rooms(counts: &[RoomUnreadCounts]) -> impl Iterator<Item = &str> {
    counts
        .iter()
        .filter(|counts| counts.notifications == 0)
        .map(|counts| counts.room_id.as_str())
}

/// Withdraw banners of the active account's rooms that were read, here or on
/// another device or client, whether or not the window is shown.
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
    for room_id in read_rooms(&counts) {
        if selection.is_current() {
            notifications.withdraw_room(&account.id, room_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(room: &str, notifications: u64, mentions: u64) -> RoomUnreadCounts {
        RoomUnreadCounts {
            room_id: room.into(),
            notifications,
            mentions,
        }
    }

    #[test]
    fn only_rooms_read_somewhere_lose_their_banners() {
        let read = vec![
            counts("!read:example.org", 0, 0),
            counts("!unread:example.org", 3, 0),
            counts("!mentioned:example.org", 1, 1),
        ];
        assert_eq!(read_rooms(&read).collect::<Vec<_>>(), ["!read:example.org"]);
        assert_eq!(read_rooms(&[]).count(), 0);
    }

    #[test]
    fn notification_reconciliation_sends_no_receipt_of_its_own() {
        // Root's single automatic read path: this module only withdraws
        // banners. Its production code names no receipt or read-marker call.
        let production = include_str!("read_markers.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for call in [
            concat!("mark_room", "_read"),
            concat!("mark_thread", "_read"),
            concat!("mark_as", "_read"),
            concat!("send_single", "_receipt"),
            concat!("send_multiple", "_receipts"),
            concat!("set_read", "_marker"),
        ] {
            assert!(!production.contains(call), "{call}");
        }
    }
}
