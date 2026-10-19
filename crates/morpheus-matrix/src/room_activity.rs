//! Typing notices, read receipts and read markers for one account selection.
//!
//! Everything goes through the pinned SDK on the account's own client and
//! sync: `Room::subscribe_to_typing_notifications`, `Room::typing_notice`,
//! `Room::load_event_receipts` and the live or thread `Timeline`'s
//! `mark_as_read`, which picks the receipt thread from the timeline's focus
//! (a live room timeline that hides thread replies sends `Main`, a thread
//! timeline sends `Thread(root)`, the fully-read marker is always
//! `Unthreaded`) and only ever moves a receipt forwards. No request is built
//! here and nothing is logged.
//!
//! Incoming names and IDs are untrusted server data: bounded here, sanitized
//! by the app, and never a verification claim.

use std::{collections::HashMap, sync::Arc, time::Duration};

use matrix_sdk::{
    Room,
    event_handler::EventHandlerDropGuard,
    ruma::{
        OwnedUserId, UserId,
        api::client::receipt::create_receipt::v3::ReceiptType as SendReceiptType,
        events::receipt::{ReceiptThread, ReceiptType},
    },
};
use matrix_sdk_ui::Timeline;
use tokio::sync::broadcast;

use crate::{AccountManager, AccountSelection, MatrixCoreError, Result, account_room};

/// People typing kept from one update; the rest are counted.
const MAX_TYPING_USERS: usize = 32;
/// Readers named in one Seen-by view; the rest are counted.
const MAX_READERS: usize = 50;
/// Events, this one and later ones, examined for one Seen-by view.
pub const MAX_RECEIPT_EVENTS: usize = 64;
/// Receipts taken from one event of one receipt thread.
const MAX_RECEIPTS_PER_EVENT: usize = 256;
/// People typing whose display names are read; the rest show their ID.
const MAX_TYPING_NAMED: usize = 4;
/// Longest name or ID kept, in characters.
const MAX_NAME_CHARS: usize = 256;
const NAME_LIMIT: Duration = Duration::from_millis(400);
const WRITE_LIMIT: Duration = Duration::from_secs(15);

/// Which receipts may say that an event was seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiptScope {
    /// An event of the room's main timeline: unthreaded and main-timeline
    /// receipts only. A thread receipt never counts here.
    Room,
    /// An event of one thread: that thread's receipts and unthreaded ones on
    /// the thread's own events only.
    Thread(String),
}

impl ReceiptScope {
    pub(crate) fn threads(&self) -> Result<Vec<ReceiptThread>> {
        Ok(match self {
            Self::Room => vec![ReceiptThread::Unthreaded, ReceiptThread::Main],
            Self::Thread(root) => vec![
                ReceiptThread::Thread(crate::parse_event_id(root)?),
                ReceiptThread::Unthreaded,
            ],
        })
    }
}

/// Someone in the room, as the server and this device's store describe them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityMember {
    pub user_id: String,
    pub name: Option<String>,
}

/// One person's public read receipt at or after an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reader {
    pub member: ActivityMember,
    /// When the receipt was made, if the server said.
    pub at_ms: Option<u64>,
    /// The receipt is on this event itself, not on a later one.
    pub on_this_event: bool,
}

/// Public read receipts at or after one event, from this device's store.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeenBy {
    pub readers: Vec<Reader>,
    /// Readers past MAX_READERS.
    pub omitted: usize,
    /// Events examined: this one and the later ones the caller listed.
    pub examined: usize,
}

/// People typing in one room, until dropped or the selection is revoked.
/// Dropping it removes the SDK event handler.
pub struct TypingWatch {
    room: Room,
    selection: AccountSelection,
    updates: broadcast::Receiver<Vec<OwnedUserId>>,
    _handler: EventHandlerDropGuard,
}

/// One typing update: who is typing, and how many more were not kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Typing {
    pub members: Vec<ActivityMember>,
    pub omitted: usize,
}

impl TypingWatch {
    /// The next complete set of other people typing. `None` once the
    /// selection is revoked or the feed closed. A lagging receiver skips to
    /// the newest set, since each update replaces the last.
    pub async fn next(&mut self) -> Option<Typing> {
        loop {
            let update = tokio::select! {
                biased;
                _ = self.selection.revoked() => return None,
                update = self.updates.recv() => update,
            };
            match update {
                Ok(users) => {
                    let typing = typing_members(&self.room, users).await;
                    return self.selection.is_current().then_some(typing);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

impl AccountManager {
    async fn activity_room(&self, selection: &AccountSelection, room_id: &str) -> Result<Room> {
        let state = self.state.lock().await;
        if !selection.is_current()
            || state.active_account_id.as_deref() != Some(selection.account_id())
        {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        account_room(&state, selection.account_id(), room_id)
    }

    /// The selected account's live room timeline (`thread_root` None) or one
    /// thread's timeline, checked against the exact selection before and
    /// after it is opened. A write started on it goes out with that
    /// account's own client; revocation afterwards does not abandon it
    /// half-way, and the caller never reports it as done for another account.
    async fn activity_timeline(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        thread_root: Option<&str>,
    ) -> Result<Arc<Timeline>> {
        let loaded = {
            let state = self.state.lock().await;
            if !selection.is_current()
                || state.active_account_id.as_deref() != Some(selection.account_id())
            {
                return Err(MatrixCoreError::SelectedAccountChanged);
            }
            crate::account_access::ensure_eligible(&state, selection.account_id())?;
            state
                .loaded
                .get(selection.account_id())
                .cloned()
                .ok_or(MatrixCoreError::SelectedAccountChanged)?
        };
        let timeline = match thread_root {
            Some(root) => loaded.thread_timeline(room_id, root).await?,
            None => loaded.timeline(room_id).await?,
        };
        ensure_current(selection)?;
        Ok(timeline)
    }

    /// Watch who is typing in one room for exactly this selection.
    pub async fn typing_watch_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
    ) -> Result<TypingWatch> {
        let room = self.activity_room(selection, room_id).await?;
        let (handler, updates) = room.subscribe_to_typing_notifications();
        Ok(TypingWatch {
            room,
            selection: selection.clone(),
            updates,
            _handler: handler,
        })
    }

    /// Start or stop this account's typing notice in one room. The SDK sends
    /// a start at most every 3 seconds (each lasts 4) and a stop only while a
    /// start is active. An error or timeout means the stop may not have
    /// arrived; the server still ends it after its timeout.
    pub async fn send_typing_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        typing: bool,
    ) -> Result<()> {
        let room = self.activity_room(selection, room_id).await?;
        ensure_current(selection)?;
        let sent = tokio::time::timeout(WRITE_LIMIT, room.typing_notice(typing)).await;
        sent.map_err(|_| MatrixCoreError::Sdk("typing notice timed out".into()))?
            .map_err(|_| MatrixCoreError::Sdk("typing notice failed".into()))
    }

    /// "Mark as read", only ever on the user's explicit choice: the
    /// fully-read marker (unthreaded) and a read receipt on the live
    /// timeline's latest event, public only when `public`. Nothing calls it
    /// automatically. Returns whether anything was sent; the SDK skips a
    /// receipt that would not move forwards. Unread counts change only when
    /// sync reports the server's result.
    pub async fn explicit_mark_read_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        public: bool,
    ) -> Result<bool> {
        let receipt = receipt_type(public);
        let timeline = self.activity_timeline(selection, room_id, None).await?;
        let marked = async {
            let marker = timeline.mark_as_read(SendReceiptType::FullyRead).await;
            let marker = marker.map_err(|_| MatrixCoreError::Sdk("read marker failed".into()))?;
            ensure_current(selection)?;
            let read = timeline.mark_as_read(receipt).await;
            let read = read.map_err(|_| MatrixCoreError::Sdk("read receipt failed".into()))?;
            Ok::<_, MatrixCoreError>(marker || read)
        };
        tokio::time::timeout(2 * WRITE_LIMIT, marked)
            .await
            .map_err(|_| MatrixCoreError::Sdk("marking as read timed out".into()))?
    }

    /// `event_id` was actually viewed in a live room timeline (`thread_root`
    /// None: receipt thread `Main`) or in one thread (`Thread(root)`): send a
    /// read receipt there, private unless `public`. The SDK skips it unless it
    /// moves this account's receipt forwards, and never targets this
    /// account's own event. No fully-read marker moves. Returns whether a
    /// receipt was sent.
    pub async fn send_viewed_receipt_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        thread_root: Option<&str>,
        event_id: &str,
        public: bool,
    ) -> Result<bool> {
        let receipt = receipt_type(public);
        let event = crate::parse_event_id(event_id)?;
        let timeline = self.activity_timeline(selection, room_id, thread_root).await?;
        let read = timeline.send_single_receipt(receipt, event);
        tokio::time::timeout(WRITE_LIMIT, read)
            .await
            .map_err(|_| MatrixCoreError::Sdk("read receipt timed out".into()))?
            .map_err(|_| MatrixCoreError::Sdk("read receipt failed".into()))
    }

    /// Other people's public read receipts on `events` (this event first,
    /// then later ones of the same scope, at most MAX_RECEIPT_EVENTS), from
    /// this device's store. Private receipts of others are never shared by
    /// the server, so they are never shown; this account is left out.
    pub async fn seen_by_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        scope: &ReceiptScope,
        events: &[String],
    ) -> Result<SeenBy> {
        let room = self.activity_room(selection, room_id).await?;
        let threads = scope.threads()?;
        let own = room.own_user_id().to_owned();
        let mut found: HashMap<OwnedUserId, (bool, Option<u64>)> = HashMap::new();
        let examined = events.len().min(MAX_RECEIPT_EVENTS);
        for (index, event) in events.iter().take(MAX_RECEIPT_EVENTS).enumerate() {
            let id = crate::parse_event_id(event)?;
            for thread in &threads {
                let receipts = room
                    .load_event_receipts(ReceiptType::Read, thread, &id)
                    .await
                    .map_err(|_| MatrixCoreError::Sdk("read receipts unavailable".into()))?;
                for (user, receipt) in receipts.into_iter().take(MAX_RECEIPTS_PER_EVENT) {
                    if user == own {
                        continue;
                    }
                    let at = receipt.ts.map(|ts| u64::from(ts.0));
                    let entry = found.entry(user).or_insert((index == 0, at));
                    entry.0 |= index == 0;
                }
            }
            ensure_current(selection)?;
        }
        let mut readers: Vec<(OwnedUserId, bool, Option<u64>)> = found
            .into_iter()
            .map(|(user, (on_this_event, at))| (user, on_this_event, at))
            .collect();
        readers.sort_by(|a, b| a.0.cmp(&b.0));
        let omitted = readers.len().saturating_sub(MAX_READERS);
        readers.truncate(MAX_READERS);
        let mut seen = SeenBy {
            readers: Vec::with_capacity(readers.len()),
            omitted,
            examined,
        };
        for (user, on_this_event, at_ms) in readers {
            seen.readers.push(Reader {
                member: member(&room, &user).await,
                at_ms,
                on_this_event,
            });
        }
        ensure_current(selection)?;
        Ok(seen)
    }
}

fn ensure_current(selection: &AccountSelection) -> Result<()> {
    if selection.is_current() {
        Ok(())
    } else {
        Err(MatrixCoreError::SelectedAccountChanged)
    }
}

fn receipt_type(public: bool) -> SendReceiptType {
    if public {
        SendReceiptType::Read
    } else {
        SendReceiptType::ReadPrivate
    }
}

fn bounded(value: &str) -> String {
    value.chars().take(MAX_NAME_CHARS).collect()
}

/// A member's local display name, read from this device's store only.
async fn member(room: &Room, user: &UserId) -> ActivityMember {
    let read = async {
        let member = room.get_member_no_sync(user).await.ok()??;
        member.display_name().map(bounded)
    };
    let name = tokio::time::timeout(NAME_LIMIT, read).await.ok().flatten();
    ActivityMember {
        user_id: bounded(user.as_str()),
        name,
    }
}

async fn typing_members(room: &Room, users: Vec<OwnedUserId>) -> Typing {
    let omitted = users.len().saturating_sub(MAX_TYPING_USERS);
    let mut members = Vec::with_capacity(users.len().min(MAX_TYPING_USERS));
    for (index, user) in users.iter().take(MAX_TYPING_USERS).enumerate() {
        members.push(if index < MAX_TYPING_NAMED {
            member(room, user).await
        } else {
            ActivityMember {
                user_id: bounded(user.as_str()),
                name: None,
            }
        });
    }
    Typing { members, omitted }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_room_event_never_counts_a_thread_receipt() {
        let room = ReceiptScope::Room.threads().unwrap();
        assert_eq!(room, [ReceiptThread::Unthreaded, ReceiptThread::Main]);
        assert!(!room.iter().any(|thread| matches!(thread, ReceiptThread::Thread(_))));
        let scope = ReceiptScope::Thread("$root:example.org".into());
        let thread = scope.threads().unwrap();
        assert!(matches!(&thread[0], ReceiptThread::Thread(root) if root == "$root:example.org"));
        // The main timeline's receipts never count for a thread's events.
        assert!(!thread.contains(&ReceiptThread::Main));
        let invalid = ReceiptScope::Thread("not an event".into());
        assert!(invalid.threads().is_err());
    }

    #[test]
    fn outgoing_receipts_are_private_unless_public_was_chosen() {
        assert_eq!(receipt_type(false), SendReceiptType::ReadPrivate);
        assert_eq!(receipt_type(true), SendReceiptType::Read);
        let long = "x".repeat(10 * MAX_NAME_CHARS);
        assert_eq!(bounded(&long).chars().count(), MAX_NAME_CHARS);
    }
}
