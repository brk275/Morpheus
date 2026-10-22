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
//! A receipt write carries an [`ActivityCancellation`]. Until the request is
//! handed to the SDK, cancelling withdraws it: the timeline lookup (which can
//! wait on the account lock and a timeline build) is dropped and nothing is
//! sent. Cancelling once the SDK has started only stops waiting for it; a
//! request the homeserver already received cannot be recalled.
//!
//! Incoming names and IDs are untrusted server data: bounded here, sanitized
//! by the app, and never a verification claim.

use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use matrix_sdk::{
    Room,
    config::RequestConfig,
    event_handler::EventHandlerDropGuard,
    ruma::{
        OwnedUserId, UserId,
        api::client::receipt::create_receipt::v3::ReceiptType as SendReceiptType,
        events::receipt::{ReceiptThread, ReceiptType},
    },
};
use matrix_sdk_ui::Timeline;
use tokio::sync::{Notify, broadcast};

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
/// Largest answer read for one explicit Mark as read receipt request.
const RECEIPT_RESPONSE_BYTES: u64 = 16 * 1024;

/// The explicit Mark as read receipts: one bounded attempt, no SDK retry and
/// no authentication refresh or replay, a capped answer.
fn receipt_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .timeout(WRITE_LIMIT)
        .max_response_bytes(RECEIPT_RESPONSE_BYTES)
        .disable_auth_refresh()
}

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

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
    /// Cancelled from this moment on, without anyone calling cancel.
    deadline: Option<Instant>,
}

/// Withdraws one queued receipt, read marker or typing write. It takes effect
/// up to the moment the request is handed to the SDK; cancelling never
/// recalls a request the homeserver may already have received.
#[derive(Clone, Default)]
pub struct ActivityCancellation(Arc<CancellationState>);

impl ActivityCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancelled at `deadline` at the latest, such as a typing start that
    /// must not go out once its idle deadline has passed.
    pub fn until(deadline: Instant) -> Self {
        Self(Arc::new(CancellationState {
            deadline: Some(deadline),
            ..CancellationState::default()
        }))
    }

    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
            || self
                .0
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// Resolves once any clone is cancelled or the deadline passes.
    pub async fn cancelled(&self) {
        // A past deadline is ready now, even before Tokio's timer wheel has
        // observed it. Otherwise an immediately-ready lookup could win and
        // report its error instead of the queued write's withdrawal.
        if self.is_cancelled() {
            return;
        }
        let expired = async {
            match self.0.deadline {
                Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                None => std::future::pending().await,
            }
        };
        let flagged = async {
            loop {
                let notified = self.0.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.0.cancelled.load(Ordering::Acquire) {
                    return;
                }
                notified.await;
            }
        };
        tokio::select! {
            () = expired => {}
            () = flagged => {}
        }
    }
}

/// What became of one receipt, read marker or typing write. A typing notice
/// handed to the SDK is `Sent` even when the SDK's own 3-second throttle sent
/// nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptOutcome {
    /// The homeserver accepted it.
    Sent,
    /// The SDK sent nothing: it would not have moved this account's receipt
    /// forwards.
    NotNeeded,
    /// Cancelled before anything was handed to the SDK: nothing was sent.
    Withdrawn,
    /// Cancelled after the SDK had started: it may or may not have reached
    /// the homeserver, and nothing waits for the answer.
    Interrupted,
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
    /// start is active. `cancellation` withdraws it up to the moment it is
    /// handed to the SDK: the room lookup waits on the account lock, so a
    /// start whose idle deadline passes while it waits never goes out. An
    /// error, timeout or interruption means a stop may not have arrived; the
    /// server still ends the start after its timeout.
    pub async fn send_typing_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        typing: bool,
        cancellation: &ActivityCancellation,
    ) -> Result<ReceiptOutcome> {
        let lookup = Box::pin(self.activity_room(selection, room_id));
        send_unless_cancelled(cancellation, lookup, |room| async move {
            ensure_current(selection)?;
            let sent = tokio::time::timeout(WRITE_LIMIT, room.typing_notice(typing)).await;
            sent.map_err(|_| MatrixCoreError::Sdk("typing notice timed out".into()))?
                .map_err(|_| MatrixCoreError::Sdk("typing notice failed".into()))?;
            Ok::<bool, MatrixCoreError>(true)
        })
        .await
    }

    /// "Mark as read", only ever on the user's explicit choice: the
    /// fully-read marker (unthreaded) and a read receipt on the live
    /// timeline's latest event, public only when `public`. Nothing calls it
    /// automatically. The SDK skips a receipt that would not move forwards.
    /// It uses the SDK's opt-in mark-as-read that never touches the room's
    /// `m.marked_unread` flag (the app's owned, lossless writer clears it),
    /// with bounded, single-attempt requests that never refresh or replay.
    /// Unread counts change only when sync reports the server's result.
    /// `cancellation` withdraws it up to the moment it is handed to the SDK.
    pub async fn explicit_mark_read_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        public: bool,
        cancellation: &ActivityCancellation,
    ) -> Result<ReceiptOutcome> {
        let receipt = receipt_type(public);
        let lookup = Box::pin(self.activity_timeline(selection, room_id, None));
        let marked = send_unless_cancelled(cancellation, lookup, |timeline| async move {
            ensure_current(selection)?;
            let marker = timeline
                .mark_as_read_without_unread_flag(
                    SendReceiptType::FullyRead,
                    Some(receipt_config()),
                )
                .await;
            let marker = marker.map_err(|_| MatrixCoreError::Sdk("read marker failed".into()))?;
            ensure_current(selection)?;
            let read = timeline
                .mark_as_read_without_unread_flag(receipt, Some(receipt_config()))
                .await;
            let read = read.map_err(|_| MatrixCoreError::Sdk("read receipt failed".into()))?;
            Ok::<bool, MatrixCoreError>(marker || read)
        });
        tokio::time::timeout(2 * WRITE_LIMIT, marked)
            .await
            .map_err(|_| MatrixCoreError::Sdk("marking as read timed out".into()))?
    }

    /// `event_id` was actually viewed in a live room timeline (`thread_root`
    /// None: receipt thread `Main`) or in one thread (`Thread(root)`): send a
    /// read receipt there, private unless `public`. The SDK skips it unless it
    /// moves this account's receipt forwards, and never targets this
    /// account's own event. No fully-read marker moves. `cancellation`
    /// withdraws it up to the moment it is handed to the SDK.
    pub async fn send_viewed_receipt_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        thread_root: Option<&str>,
        event_id: &str,
        public: bool,
        cancellation: &ActivityCancellation,
    ) -> Result<ReceiptOutcome> {
        let receipt = receipt_type(public);
        let event = crate::parse_event_id(event_id)?;
        let lookup = Box::pin(self.activity_timeline(selection, room_id, thread_root));
        send_unless_cancelled(cancellation, lookup, |timeline| async move {
            ensure_current(selection)?;
            let read = timeline.send_single_receipt(receipt, event);
            tokio::time::timeout(WRITE_LIMIT, read)
                .await
                .map_err(|_| MatrixCoreError::Sdk("read receipt timed out".into()))?
                .map_err(|_| MatrixCoreError::Sdk("read receipt failed".into()))
        })
        .await
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

/// Looks up what a write needs, then writes, unless `cancellation` fires
/// first. While the lookup runs, cancelling drops it and nothing is sent. The
/// last check is made right before `send` hands the request to the SDK; once
/// it has, cancelling only stops waiting, because a request the homeserver
/// may already have received cannot be recalled.
async fn send_unless_cancelled<T, L, S, F>(
    cancellation: &ActivityCancellation,
    lookup: L,
    send: S,
) -> Result<ReceiptOutcome>
where
    L: Future<Output = Result<T>>,
    S: FnOnce(T) -> F,
    F: Future<Output = Result<bool>>,
{
    let found = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Ok(ReceiptOutcome::Withdrawn),
        found = lookup => found,
    };
    // Cancellation also wins when the lookup completes at the same time,
    // including when its result is an error. No SDK work has started yet.
    if cancellation.is_cancelled() {
        return Ok(ReceiptOutcome::Withdrawn);
    }
    let found = found?;
    let sending = send(found);
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Ok(ReceiptOutcome::Interrupted),
        sent = sending => Ok(if sent? {
            ReceiptOutcome::Sent
        } else {
            ReceiptOutcome::NotNeeded
        }),
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
    use crate::history::tests::{fixture, runtime};
    use futures_util::task::noop_waker_ref;
    use std::{
        pin::pin,
        sync::atomic::AtomicUsize,
        task::{Context, Poll},
    };

    /// Mark as read never asks the SDK to change the unread flag: no default
    /// mark-as-read or receipt-set call that unsets it, only the opt-in one
    /// with a bounded config that neither retries nor refreshes or replays.
    #[test]
    fn mark_as_read_leaves_the_unread_flag_to_its_owned_writer() {
        let source = include_str!("room_activity.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains(".mark_as_read("));
        assert!(!production.contains("send_multiple_receipts"));
        assert!(!production.contains("set_unread_flag"));
        assert_eq!(
            production
                .matches(".mark_as_read_without_unread_flag(")
                .count(),
            2
        );
        let config = format!("{:?}", receipt_config());
        assert!(config.contains("retry_limit: Some(0)") || config.contains("retry_limit: 0"));
        assert!(config.contains("disable_auth_refresh: true"), "{config}");
        assert!(config.contains("max_response_bytes"), "{config}");
    }

    #[test]
    fn a_receipt_waiting_for_its_timeline_is_withdrawn_before_it_is_sent() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let room = "!activity:example.org";
            let event = "$viewed:example.org";
            // Holding the account lock delays the timeline lookup, as an account
            // transition or a slow timeline build does.
            let state = manager.state.lock().await;
            let cancel = ActivityCancellation::new();
            let mut write = Box::pin(
                manager.send_viewed_receipt_selected(&selection, room, None, event, true, &cancel),
            );
            let mut context = Context::from_waker(noop_waker_ref());
            assert!(write.as_mut().poll(&mut context).is_pending());
            // Leaving the view, losing focus or turning receipts off revokes it
            // while the selection stays current.
            cancel.cancel();
            let outcome = tokio::time::timeout(Duration::from_secs(2), write).await;
            assert_eq!(outcome.unwrap().unwrap(), ReceiptOutcome::Withdrawn);
            drop(state);
            // Not cancelled, the same write runs on to the room lookup, so the
            // withdrawal above came before anything reached the SDK.
            let open = ActivityCancellation::new();
            let reached = manager
                .send_viewed_receipt_selected(&selection, room, None, event, true, &open)
                .await;
            assert!(matches!(reached, Err(MatrixCoreError::RoomNotFound(_))));
            drop(manager);
            std::fs::remove_dir_all(directory).unwrap();
        });
    }

    #[test]
    fn a_typing_start_waiting_for_the_account_lock_expires_at_its_idle_deadline() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let room = "!typing:example.org";
            // The account lock is held past the start's idle deadline.
            let state = manager.state.lock().await;
            let deadline = Instant::now() + Duration::from_millis(20);
            let expiring = ActivityCancellation::until(deadline);
            let start = manager.send_typing_selected(&selection, room, true, &expiring);
            let outcome = tokio::time::timeout(Duration::from_secs(2), start).await;
            assert_eq!(outcome.unwrap().unwrap(), ReceiptOutcome::Withdrawn);
            assert!(expiring.is_cancelled());
            drop(state);
            // A deadline that already passed hands nothing over, lock or not.
            let elapsed = ActivityCancellation::until(Instant::now());
            let late = manager
                .send_typing_selected(&selection, room, true, &elapsed)
                .await;
            assert_eq!(late.unwrap(), ReceiptOutcome::Withdrawn);
            // Without a deadline the same start runs on to the room lookup, so
            // the withdrawals above came before anything reached the SDK.
            let open = ActivityCancellation::new();
            let reached = manager
                .send_typing_selected(&selection, room, true, &open)
                .await;
            assert!(matches!(reached, Err(MatrixCoreError::RoomNotFound(_))));
            drop(manager);
            std::fs::remove_dir_all(directory).unwrap();
        });
    }

    #[test]
    fn nothing_is_handed_to_the_sdk_once_a_write_is_cancelled() {
        let mut context = Context::from_waker(noop_waker_ref());
        let handed = AtomicUsize::new(0);
        // An already elapsed idle deadline is ready on its first poll, with
        // no timer tick or runtime needed. Neither lookup nor send is polled.
        let expired = ActivityCancellation::until(Instant::now());
        let looked_up = AtomicUsize::new(0);
        let immediate = std::future::poll_fn(|_| {
            looked_up.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Err::<(), _>(MatrixCoreError::SelectedAccountChanged))
        });
        let mut write = pin!(send_unless_cancelled(&expired, immediate, |()| {
            handed.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(true))
        }));
        let polled = write.as_mut().poll(&mut context);
        assert!(matches!(polled, Poll::Ready(Ok(ReceiptOutcome::Withdrawn))));
        assert_eq!(looked_up.load(Ordering::SeqCst), 0);
        // Cancelled while the lookup waits: withdrawn, never handed over.
        let cancel = ActivityCancellation::new();
        let waiting = std::future::pending::<Result<()>>();
        let mut write = pin!(send_unless_cancelled(&cancel, waiting, |()| {
            handed.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(true))
        }));
        assert!(write.as_mut().poll(&mut context).is_pending());
        cancel.cancel();
        let polled = write.as_mut().poll(&mut context);
        assert!(matches!(polled, Poll::Ready(Ok(ReceiptOutcome::Withdrawn))));
        // Cancelled as the lookup completes: the last check before the SDK
        // withdraws it.
        let cancel = ActivityCancellation::new();
        let completing = std::future::poll_fn(|_| {
            cancel.cancel();
            Poll::Ready(Ok(()))
        });
        let mut write = pin!(send_unless_cancelled(&cancel, completing, |()| {
            handed.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(true))
        }));
        let polled = write.as_mut().poll(&mut context);
        assert!(matches!(polled, Poll::Ready(Ok(ReceiptOutcome::Withdrawn))));
        assert_eq!(handed.load(Ordering::SeqCst), 0);
        // A lookup error completing with cancellation is still withdrawn.
        // A real account error without cancellation remains an account error.
        for cancelled in [true, false] {
            let cancel = ActivityCancellation::new();
            let completing = std::future::poll_fn(|_| {
                if cancelled {
                    cancel.cancel();
                }
                Poll::Ready(Err::<(), _>(MatrixCoreError::SelectedAccountChanged))
            });
            let mut write = pin!(send_unless_cancelled(&cancel, completing, |()| {
                handed.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(true))
            }));
            let polled = write.as_mut().poll(&mut context);
            if cancelled {
                assert!(matches!(polled, Poll::Ready(Ok(ReceiptOutcome::Withdrawn))));
            } else {
                assert!(matches!(
                    polled,
                    Poll::Ready(Err(MatrixCoreError::SelectedAccountChanged))
                ));
            }
        }
        assert_eq!(handed.load(Ordering::SeqCst), 0);
        // Cancelled after the SDK started: only reported as interrupted.
        let cancel = ActivityCancellation::new();
        let found = std::future::ready(Ok(()));
        let mut write = pin!(send_unless_cancelled(&cancel, found, |()| {
            handed.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<Result<bool>>()
        }));
        assert!(write.as_mut().poll(&mut context).is_pending());
        assert_eq!(handed.load(Ordering::SeqCst), 1);
        cancel.cancel();
        let polled = write.as_mut().poll(&mut context);
        assert!(matches!(
            polled,
            Poll::Ready(Ok(ReceiptOutcome::Interrupted))
        ));
        // Not cancelled: the SDK's own answer.
        for (sent, outcome) in [
            (true, ReceiptOutcome::Sent),
            (false, ReceiptOutcome::NotNeeded),
        ] {
            let cancel = ActivityCancellation::new();
            let found = std::future::ready(Ok(()));
            let write = pin!(send_unless_cancelled(&cancel, found, |()| {
                std::future::ready(Ok(sent))
            }));
            let polled = write.poll(&mut context);
            assert!(matches!(polled, Poll::Ready(Ok(got)) if got == outcome));
        }
    }

    #[test]
    fn a_room_event_never_counts_a_thread_receipt() {
        let room = ReceiptScope::Room.threads().unwrap();
        // Exactly these two: never a thread receipt.
        assert_eq!(room, [ReceiptThread::Unthreaded, ReceiptThread::Main]);
        let scope = ReceiptScope::Thread("$root:example.org".into());
        let thread = scope.threads().unwrap();
        let root = crate::parse_event_id("$root:example.org").unwrap();
        assert_eq!(
            thread,
            [ReceiptThread::Thread(root), ReceiptThread::Unthreaded]
        );
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
