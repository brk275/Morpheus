//! Read position of the conversation on screen.
//!
//! A receipt is sent only for one event the caller saw: a private read
//! receipt (`m.read.private`) on the same cached timeline that produced the
//! row, so other room members never see it. No timeline is built for a
//! receipt; the event must be a remote event of that timeline, after the
//! last receipt this client confirmed there. The SDK additionally refuses to
//! move the read position backwards and never targets the account's own
//! events. Nothing here sends a public receipt or a fully-read marker, and a
//! refused private receipt is never retried as a public one. Every entry
//! point is bound to the caller's `AccountSelection` like the notification
//! rules; a receipt already handed to the homeserver cannot be recalled by a
//! later switch.

use std::sync::Arc;

use eyeball_im::Vector;
use matrix_sdk::ruma::{EventId, api::client::receipt::create_receipt::v3::ReceiptType};
use matrix_sdk_ui::timeline::TimelineItem;

use crate::{AccountManager, AccountSelection, MatrixCoreError, Result, parse_event_id};

/// Where the account's unread messages begin in a room's loaded live timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnreadStart {
    /// The first loaded message from someone else after the read position.
    Event(String),
    /// No message from someone else follows the read position.
    AllRead,
    /// The read position, this event, is not in the loaded history and the
    /// room reports unread messages, so they begin before the loaded history.
    BeforeLoaded(String),
    /// The account has no known read position in this room.
    Unknown,
}

/// A private read receipt for one event the user saw.
#[derive(Clone, Copy, Debug)]
pub struct SeenReceipt<'a> {
    pub room_id: &'a str,
    /// The thread rooted at this event; `None` for the room's live timeline.
    pub thread_root: Option<&'a str>,
    pub event_id: &'a str,
    /// The last receipt this client confirmed in the same timeline. The new
    /// one is sent only when its event comes after that one.
    pub after: Option<&'a str>,
    /// The newest row was seen too. Events after `event_id` that the
    /// timeline shows no row for (undecryptable messages, polls, calls,
    /// redactions) then count as read: the receipt moves to the latest
    /// event, so the room's unread count can reach zero.
    pub through_latest: bool,
}

/// What became of a receipt for a seen event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptOutcome {
    /// The homeserver accepted the receipt.
    Sent,
    /// Nothing was sent: the read position is already there or later, or the
    /// event is not in the timeline that showed it, or that timeline is gone.
    Skipped,
    /// The homeserver refused it for good (a client error other than rate
    /// limiting); sending it again would not help.
    Refused,
}

impl AccountManager {
    /// Send a private read receipt for an event the user saw in the room's
    /// live timeline, or in one thread's timeline.
    pub async fn send_seen_receipt_selected(
        &self,
        selection: &AccountSelection,
        receipt: SeenReceipt<'_>,
    ) -> Result<ReceiptOutcome> {
        let event_id = parse_event_id(receipt.event_id)?;
        let after = receipt.after.map(parse_event_id).transpose()?;
        let key = match receipt.thread_root {
            Some(root) => format!("thread:{}:{root}", receipt.room_id),
            None => receipt.room_id.to_owned(),
        };
        let through_latest = receipt.through_latest;
        let loaded = self.rule_loaded(selection).await?;
        self.under_rule_selection(selection, async move {
            // Only the timeline that produced the row; never build one here.
            let Some(timeline) = loaded.timelines.lock().await.get(&key).cloned() else {
                return Ok(ReceiptOutcome::Skipped);
            };
            let items = timeline.items().await;
            let Some(position) = remote_position(&items, &event_id) else {
                return Ok(ReceiptOutcome::Skipped);
            };
            let earlier = after
                .as_deref()
                .and_then(|after| remote_position(&items, after))
                .is_some_and(|previous| previous >= position);
            if earlier {
                return Ok(ReceiptOutcome::Skipped);
            }
            let later_message = items
                .iter()
                .skip(position + 1)
                .any(|item| foreign_message(item));
            let mut target = event_id;
            if through_latest
                && !later_message
                && let Some(latest) = timeline.latest_event_id().await
                && remote_position(&items, &latest).is_some_and(|latest| latest > position)
            {
                target = latest;
            }
            match timeline
                .send_single_receipt(ReceiptType::ReadPrivate, target)
                .await
            {
                Ok(true) => Ok(ReceiptOutcome::Sent),
                Ok(false) => Ok(ReceiptOutcome::Skipped),
                Err(error) if refused(&error) => Ok(ReceiptOutcome::Refused),
                Err(error) => Err(MatrixCoreError::Sdk(error.to_string())),
            }
        })
        .await
    }

    /// Where unread messages begin in the room's live timeline. The read
    /// position is `read` when given (the one an earlier answer reported, so
    /// receipts sent since do not move it), else the room's latest unthreaded
    /// or main-thread receipt, public or private: the position its unread
    /// count starts from. Reads local state only.
    pub async fn unread_start_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        read: Option<&str>,
    ) -> Result<UnreadStart> {
        let given = read.map(parse_event_id).transpose()?;
        let loaded = self.rule_loaded(selection).await?;
        self.under_rule_selection(selection, async move {
            let Some(timeline) = loaded.timelines.lock().await.get(room_id).cloned() else {
                return Ok(UnreadStart::Unknown);
            };
            let room = timeline.room();
            let read = given.or_else(|| {
                room.read_receipts()
                    .latest_active
                    .map(|receipt| receipt.event_id)
            });
            let Some(read) = read else {
                return Ok(UnreadStart::Unknown);
            };
            let items = timeline.items().await;
            Ok(unread_start_in(&items, &read, room.num_unread_messages()))
        })
        .await
    }
}

/// A client error other than rate limiting: the request itself is refused.
fn refused(error: &matrix_sdk::Error) -> bool {
    error.as_client_api_error().is_some_and(|error| {
        let status = error.status_code.as_u16();
        (400..500).contains(&status) && status != 429
    })
}

fn remote_position(items: &Vector<Arc<TimelineItem>>, event_id: &EventId) -> Option<usize> {
    items.iter().position(|item| {
        item.as_event()
            .is_some_and(|event| event.is_remote_event() && event.event_id() == Some(event_id))
    })
}

/// A remote message or sticker from someone else: what the timeline shows as
/// another member's row.
fn foreign_message(item: &TimelineItem) -> bool {
    item.as_event().is_some_and(|event| {
        let content = event.content();
        let shown = content.as_message().is_some() || content.as_sticker().is_some();
        shown && event.is_remote_event() && !event.is_own()
    })
}

fn unread_start_in(
    items: &Vector<Arc<TimelineItem>>,
    read: &EventId,
    unread_messages: u64,
) -> UnreadStart {
    let position = items
        .iter()
        .position(|item| item.as_event().and_then(|event| event.event_id()) == Some(read));
    let Some(position) = position else {
        return if unread_messages > 0 {
            UnreadStart::BeforeLoaded(read.to_string())
        } else {
            UnreadStart::AllRead
        };
    };
    items
        .iter()
        .skip(position + 1)
        .filter(|item| foreign_message(item))
        .find_map(|item| item.as_event().and_then(|event| event.event_id()))
        .map_or(UnreadStart::AllRead, |event_id| {
            UnreadStart::Event(event_id.to_string())
        })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::PathBuf, time::Duration};

    use matrix_sdk::Client;
    use tokio::sync::{Mutex, watch};
    use uuid::Uuid;

    use super::*;
    use crate::{AccountInfo, LoadedAccount, RecoveryGate};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    // No authenticated session, vault entry, sync or persistence task.
    async fn fixture() -> (PathBuf, Arc<AccountManager>, AccountInfo) {
        let directory =
            std::env::temp_dir().join(format!("morpheus-unread-read-{}", Uuid::new_v4()));
        let manager = Arc::new(AccountManager::open(&directory).unwrap());
        let account = AccountInfo {
            id: Uuid::new_v4().to_string(),
            user_id: "@casey:example.org".into(),
            device_id: "UNREAD-FIXTURE".into(),
            homeserver: "http://127.0.0.1:9".into(),
        };
        let client = Client::builder()
            .homeserver_url(&account.homeserver)
            .build()
            .await
            .unwrap();
        client.send_queue().set_enabled(false).await;
        let loaded = Arc::new(LoadedAccount {
            client,
            timelines: Mutex::new(HashMap::new()),
            composed_edits: Mutex::new(HashMap::new()),
            pending_sends: Mutex::new(HashMap::new()),
            originals: Mutex::new(HashMap::new()),
            recovered: Mutex::new(Vec::new()),
            recovery_gate: Mutex::new(RecoveryGate::default()),
            recovery_ready: watch::channel(false).0,
            incoming_verification_tx: watch::channel(Vec::new()).0,
            notification_handler_registered: Mutex::new(false),
            token_task: Mutex::new(None),
        });
        {
            let mut state = manager.state.lock().await;
            state.registry.accounts.push(account.clone());
            state.registry.active_account_id = Some(account.id.clone());
            state.active_account_id = Some(account.id.clone());
            state.loaded.insert(account.id.clone(), loaded);
        }
        manager.publish_active_account(Some(account.clone()));
        (directory, manager, account)
    }

    #[test]
    fn receipts_never_outlive_their_selection_or_build_a_timeline() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture().await;
            let stale = manager.account_selection(&account.id).unwrap();
            // A -> none -> A: the same account is visible again under a new
            // selection, so the earlier one must not send or read anything.
            manager.publish_active_account(None);
            manager.publish_active_account(Some(account.clone()));
            assert!(!stale.is_current());
            let receipt = SeenReceipt {
                room_id: "!room:example.org",
                thread_root: None,
                event_id: "$event:example.org",
                after: None,
                through_latest: true,
            };
            let wait = Duration::from_secs(2);
            let sent = manager.send_seen_receipt_selected(&stale, receipt);
            let sent = tokio::time::timeout(wait, sent).await.unwrap();
            assert!(matches!(sent, Err(MatrixCoreError::SelectedAccountChanged)));
            let start = manager.unread_start_selected(&stale, receipt.room_id, None);
            let start = tokio::time::timeout(wait, start).await.unwrap();
            assert!(matches!(start, Err(MatrixCoreError::SelectedAccountChanged)));
            // The current selection with no timeline for the room sends
            // nothing and creates no timeline.
            let current = manager.account_selection(&account.id).unwrap();
            let skipped = manager.send_seen_receipt_selected(&current, receipt);
            let skipped = tokio::time::timeout(wait, skipped).await.unwrap();
            assert!(matches!(skipped, Ok(ReceiptOutcome::Skipped)));
            let loaded = manager.state.lock().await.loaded[&account.id].clone();
            assert!(loaded.timelines.lock().await.is_empty());
            let invalid = SeenReceipt {
                event_id: "event",
                ..receipt
            };
            let invalid = manager.send_seen_receipt_selected(&current, invalid);
            let invalid = tokio::time::timeout(wait, invalid).await.unwrap();
            assert!(matches!(invalid, Err(MatrixCoreError::InvalidEventId(_))));
            let _ = std::fs::remove_dir_all(directory);
        });
    }
}
