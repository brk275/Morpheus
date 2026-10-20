//! Leaving one joined room, exactly.
//!
//! A leave is one typed `POST /rooms/{roomId}/leave` for the room ID the
//! person reviewed, and for that room alone. It never goes through
//! `Room::leave`, which also leaves every joined or invited predecessor room
//! it finds by walking `m.room.create` predecessor links (state other people
//! write, followed without a bound) and then marks the room left on this
//! device before any sync says so. Here nothing local changes: the account's
//! sync brings the new membership into the store, and the room list follows
//! it.
//!
//! The request is sent at most once: no HTTP retry (`retry_limit(0)`) and no
//! automatic authentication refresh and replay (`disable_auth_refresh`;
//! `retry_limit(0)` alone does not stop the SDK from refreshing an expired
//! access token and sending the request again), with a 30 s timeout and a
//! 64 KiB response cap. The pinned SDK's `disable_auth_refresh` option keeps
//! an expired token from triggering an automatic authentication replay.
//!
//! Every step runs under the exact `AccountSelection` that asked for it and
//! the dialog's cancellation: each wait races the cancellation first, then
//! `AccountSelection::guard` (which, from root's d56ba8b, lets a ready
//! revocation win before the work is polled). Until the request is handed to
//! the SDK, either ends the leave with nothing sent. From then on both still
//! end the wait, and the leave ends as `Unknown` (it may or may not have
//! reached the homeserver): never as left or as not left, and it is never
//! sent again. A homeserver refusal (4xx) is final for that attempt.
//! Errors use fixed text and never pass homeserver text on; nothing is
//! logged.

use std::{
    fmt,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use matrix_sdk::{
    Client, HttpError, Room, RoomState,
    config::RequestConfig,
    event_handler::EventHandlerHandle,
    ruma::{
        OwnedRoomId, RoomId,
        api::{client::membership::leave_room, error::ErrorKind},
        events::room::member::{MembershipState, SyncRoomMemberEvent},
    },
    sync::RoomUpdate,
};
use tokio::{
    sync::{Notify, broadcast, watch},
    time,
};

use crate::{AccountManager, AccountSelection, account_access, pinned_messages::plain_text};

/// Longest room name shown for review; other people's text.
pub const MAX_ROOM_LEAVE_NAME_CHARS: usize = 120;
/// Bound on reading this device's copy of the room for review.
const REVIEW_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on the leave request's one HTTP attempt.
const LEAVE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on the whole hand-off, including the SDK's version check.
const SEND_BOUND: Duration = Duration::from_secs(40);
/// Most bytes accepted in the answer (it carries `{}` or an error).
const MAX_LEAVE_RESPONSE_BYTES: u64 = 64 * 1024;
/// Longest wait for the account's sync to show the room left.
const SYNC_WAIT: Duration = Duration::from_secs(30);

/// Why a leave did not go ahead or its result is not known. The text is fixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RoomLeaveError {
    #[error("This dialog closed before anything was sent")]
    Cancelled,
    #[error("The selected account changed before anything was sent")]
    AccountChanged,
    #[error("That is not a Matrix room ID")]
    InvalidRoom,
    #[error("This account is not joined to that room on this device")]
    NotJoined,
    #[error("This room's details could not be read on this device")]
    Unavailable,
    #[error("The leave could not start in time. Nothing was sent")]
    BeforeSendTimeout,
    #[error("The room, account, security or call state changed. Nothing was sent")]
    Blocked,
    #[error("The homeserver is limiting requests; you did not leave. Wait, then try again")]
    RateLimited,
    #[error("This sign-in needs renewing; the homeserver did not apply the leave")]
    SignInExpired,
    #[error("The homeserver refused this leave request")]
    Refused,
    #[error("No answer came back; you may or may not have left this room")]
    Unknown,
}

pub type Result<T> = std::result::Result<T, RoomLeaveError>;

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

/// Ends one Leave dialog's work. Before the request is handed to the SDK,
/// nothing is sent; after that, the leave's result becomes `Unknown`.
#[derive(Clone, Default)]
pub struct RoomLeaveCancellation(Arc<CancellationState>);

impl RoomLeaveCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    /// Completes once cancelled, at once when that already happened.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            // Register before checking: notify_waiters keeps no permit.
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

impl fmt::Debug for RoomLeaveCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RoomLeaveCancellation { .. }")
    }
}

/// What leaving one room means, from this device's copy of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomLeaveReview {
    /// The exact room ID a confirmation leaves.
    pub room_id: String,
    /// The room's display name: other people's text, plain, bounded and not
    /// verified. Empty when the room has none.
    pub name: String,
    pub is_space: bool,
    pub is_direct: bool,
    /// This room replaced an earlier one, which is not left.
    pub has_predecessor: bool,
    /// A newer room replaced this one, which is not left.
    pub has_successor: bool,
    /// Joined members as of the last sync, this account included.
    pub joined_members: u64,
}

/// A leave the homeserver accepted, before this device's sync has shown it.
pub struct RoomLeaveAccepted {
    client: Client,
    room_id: OwnedRoomId,
    updates: broadcast::Receiver<RoomUpdate>,
}

impl fmt::Debug for RoomLeaveAccepted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RoomLeaveAccepted { .. }")
    }
}

/// Scalar evidence from this exact account/room's sync. SDK broadcast updates
/// contain Room (and Client/store handles), so no such receiver is retained.
/// The weak owner cannot keep a removed account's SQLite store open.
pub struct RoomLeaveObservation {
    owner: Weak<crate::LoadedAccount>,
    handler: EventHandlerHandle,
    left: watch::Receiver<bool>,
}

impl fmt::Debug for RoomLeaveObservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RoomLeaveObservation { .. }")
    }
}

impl Drop for RoomLeaveObservation {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            // Synchronous removal only: no network, store read or await.
            owner.client.remove_event_handler(self.handler.clone());
        }
    }
}

impl RoomLeaveObservation {
    fn new(client: &Client, owner: Weak<crate::LoadedAccount>, room_id: &RoomId) -> Result<Self> {
        let own = client
            .user_id()
            .ok_or(RoomLeaveError::AccountChanged)?
            .to_owned();
        let (left, updates) = watch::channel(false);
        let handler = client.add_room_event_handler(
            room_id,
            move |event: SyncRoomMemberEvent, room: Room| {
                // Pinned SDK call_sync_response_handlers runs after state is
                // persisted. Neither event nor Room enters the watch channel
                // or the returned future; only a bool survives this callback.
                if *event.state_key() == own {
                    let left_now = matches!(
                        event.membership(),
                        MembershipState::Leave | MembershipState::Ban
                    ) && matches!(room.state(), RoomState::Left | RoomState::Banned);
                    left.send_replace(left_now);
                }
                std::future::ready(())
            },
        );
        Ok(Self {
            owner,
            handler,
            left: updates,
        })
    }

    /// Acknowledgment, view close, selection revocation and channel closure
    /// are not evidence. A later own-member join resets the scalar as well.
    pub async fn left_from_sync(mut self, cancellation: &RoomLeaveCancellation) -> bool {
        loop {
            if cancellation.is_cancelled() {
                return false;
            }
            let left = *self.left.borrow_and_update();
            if left {
                return true;
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return false,
                changed = self.left.changed() => {
                    if changed.is_err() {
                        return false;
                    }
                }
            }
        }
    }
}

impl RoomLeaveAccepted {
    /// Whether this device's sync shows the room left, waiting at most 30 s
    /// under the same selection and cancellation. False when it does not
    /// (yet): the room then stays listed until a later sync. Nothing local
    /// is changed here either.
    pub async fn left_after_sync(
        mut self,
        selection: &AccountSelection,
        cancellation: &RoomLeaveCancellation,
    ) -> bool {
        let left = membership_left(&self.client, &self.room_id, &mut self.updates);
        let left = time::timeout(SYNC_WAIT, left);
        tokio::select! {
            biased;
            () = cancellation.cancelled() => false,
            left = selection.guard(left) => left.is_some_and(|left| left.unwrap_or(false)),
        }
    }
}

impl AccountManager {
    /// What leaving `room_id` means, from this device's copy of the room. No
    /// request is made.
    pub async fn room_leave_review_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        cancellation: &RoomLeaveCancellation,
    ) -> Result<RoomLeaveReview> {
        let room_id = exact_room_id(room_id)?;
        let (client, _) = self.leave_client(selection, cancellation).await?;
        let room = joined_room(&client, &room_id)?;
        let details = async {
            let name = room.display_name().await.ok().map(|name| name.to_string());
            Ok((name, room.is_direct().await.unwrap_or(false)))
        };
        let (name, is_direct) = guarded(selection, cancellation, REVIEW_TIMEOUT, details).await?;
        Ok(RoomLeaveReview {
            room_id: room_id.to_string(),
            name: plain_text(name.as_deref().unwrap_or(""), MAX_ROOM_LEAVE_NAME_CHARS),
            is_space: room.is_space(),
            is_direct,
            has_predecessor: room.predecessor_room().is_some(),
            has_successor: room.successor_room().is_some(),
            joined_members: room.joined_members_count(),
        })
    }

    /// Leave exactly `room_id`, a room this selection is joined to on this
    /// device: one leave request for that room alone (see the module notes).
    /// `Ok` means the homeserver accepted it; nothing local has changed, and
    /// `RoomLeaveAccepted::left_after_sync` waits for sync to show it.
    /// `admit` runs synchronously after all acquisition, immediately before
    /// SDK handoff. The app must recheck its view/security/call authority and
    /// retain the observation and exclusion even when this attempt is Unknown.
    /// A caller accepting every admission supplies no such app-level policy.
    pub async fn leave_room_exactly_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        cancellation: &RoomLeaveCancellation,
        admit: impl Fn(RoomLeaveObservation) -> bool + Send + Sync,
    ) -> Result<RoomLeaveAccepted> {
        let room_id = exact_room_id(room_id)?;
        let (client, owner) = self.leave_client(selection, cancellation).await?;
        joined_room(&client, &room_id)?;
        // Subscribed before the request, so the sync that brings the leave
        // cannot be missed.
        let updates = client.subscribe_to_room_updates(&room_id);
        let started = AtomicBool::new(false);
        let sending = async {
            // All awaited acquisition is over. The caller serializes this
            // admission with its view withdrawal and checks its own guards.
            scope_error(selection, cancellation)?;
            if !self.owns_selection(selection) {
                return Err(RoomLeaveError::AccountChanged);
            }
            if !admit(RoomLeaveObservation::new(&client, owner, &room_id)?) {
                return Err(RoomLeaveError::Blocked);
            }
            started.store(true, Ordering::Release);
            leave_request(&client, &room_id)
                .await
                .map_err(|error| leave_error(&error))
        };
        let interrupted = |before| {
            if started.load(Ordering::Acquire) {
                RoomLeaveError::Unknown
            } else {
                before
            }
        };
        let request = time::timeout(SEND_BOUND, sending);
        let sent = tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(interrupted(RoomLeaveError::Cancelled)),
            sent = selection.guard(request) => match sent {
                Some(Ok(sent)) => sent,
                // Revoked once handed over, or no answer in time.
                None => Err(interrupted(RoomLeaveError::AccountChanged)),
                Some(Err(_)) => Err(interrupted(RoomLeaveError::BeforeSendTimeout)),
            },
        };
        sent?;
        Ok(RoomLeaveAccepted {
            client,
            room_id,
            updates,
        })
    }

    /// The exact selection's client, while it still owns its account.
    async fn leave_client(
        &self,
        selection: &AccountSelection,
        cancellation: &RoomLeaveCancellation,
    ) -> Result<(Client, Weak<crate::LoadedAccount>)> {
        scope_error(selection, cancellation)?;
        if !self.owns_selection(selection) {
            return Err(RoomLeaveError::AccountChanged);
        }
        let state = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(RoomLeaveError::Cancelled),
            state = selection.guard(time::timeout(REVIEW_TIMEOUT, self.state.lock())) => state,
        };
        let state = state
            .ok_or(RoomLeaveError::AccountChanged)?
            .map_err(|_| RoomLeaveError::BeforeSendTimeout)?;
        if !self.owns_selection(selection) {
            return Err(RoomLeaveError::AccountChanged);
        }
        account_access::ensure_eligible(&state, selection.account_id())
            .map_err(|_| RoomLeaveError::AccountChanged)?;
        let loaded = crate::selected_loaded_for(&state, selection.account_id())
            .map_err(|_| RoomLeaveError::AccountChanged)?;
        Ok((loaded.client.clone(), Arc::downgrade(loaded)))
    }
}

/// The room ID exactly as given. An alias, a link or anything else is
/// refused, never resolved.
fn exact_room_id(room_id: &str) -> Result<OwnedRoomId> {
    RoomId::parse(room_id).map_err(|_| RoomLeaveError::InvalidRoom)
}

/// The room, while this device's copy says the account is joined to it.
fn joined_room(client: &Client, room_id: &RoomId) -> Result<Room> {
    let room = client.get_room(room_id).ok_or(RoomLeaveError::NotJoined)?;
    if room.state() != RoomState::Joined {
        return Err(RoomLeaveError::NotJoined);
    }
    Ok(room)
}

fn joined(client: &Client, room_id: &RoomId) -> bool {
    client
        .get_room(room_id)
        .is_some_and(|room| room.state() == RoomState::Joined)
}

/// Waits for a sync that shows the room no longer joined. False only when
/// the account's room updates end.
async fn membership_left(
    client: &Client,
    room_id: &RoomId,
    updates: &mut broadcast::Receiver<RoomUpdate>,
) -> bool {
    loop {
        if !joined(client, room_id) {
            return true;
        }
        // A lagged receiver only missed updates; the state is read again.
        if let Err(broadcast::error::RecvError::Closed) = updates.recv().await {
            return false;
        }
    }
}

/// Sent at most once: no HTTP retry, no authentication refresh and replay,
/// in bounded time and size.
fn leave_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .disable_auth_refresh()
        .timeout(LEAVE_TIMEOUT)
        .max_response_bytes(MAX_LEAVE_RESPONSE_BYTES)
}

/// The one leave request: `POST /rooms/{roomId}/leave` for `room_id` alone,
/// with no reason. Nothing else is visited, left or changed.
pub(crate) async fn leave_request(
    client: &Client,
    room_id: &RoomId,
) -> std::result::Result<(), HttpError> {
    client
        .send(leave_room::v3::Request::new(room_id.to_owned()))
        .with_request_config(leave_config())
        .await
        .map(|_| ())
}

/// A homeserver answer below 500 means the leave was not applied; anything
/// else (no answer, a server error, an unreadable answer) may have applied.
pub(crate) fn leave_error(error: &HttpError) -> RoomLeaveError {
    let refused = error
        .as_client_api_error()
        .is_some_and(|answer| answer.status_code.is_client_error());
    match error.client_api_error_kind() {
        Some(ErrorKind::LimitExceeded(_)) if refused => RoomLeaveError::RateLimited,
        Some(ErrorKind::UnknownToken(_)) if refused => RoomLeaveError::SignInExpired,
        Some(_) if refused => RoomLeaveError::Refused,
        _ => RoomLeaveError::Unknown,
    }
}

fn scope_error(selection: &AccountSelection, cancellation: &RoomLeaveCancellation) -> Result<()> {
    if cancellation.is_cancelled() {
        return Err(RoomLeaveError::Cancelled);
    }
    if !selection.is_current() {
        return Err(RoomLeaveError::AccountChanged);
    }
    Ok(())
}

/// Local work under the selection and the cancellation, in bounded time.
async fn guarded<T>(
    selection: &AccountSelection,
    cancellation: &RoomLeaveCancellation,
    limit: Duration,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    scope_error(selection, cancellation)?;
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(RoomLeaveError::Cancelled),
        result = selection.guard(time::timeout(limit, work)) => match result {
            Some(result) => result.unwrap_or(Err(RoomLeaveError::Unavailable)),
            None => Err(RoomLeaveError::AccountChanged),
        },
    };
    let result = result?;
    scope_error(selection, cancellation)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::tests::{fixture, runtime};
    use matrix_sdk::config::SyncSettings;
    use serde_json::{Map, Value, json};
    use std::{
        collections::VecDeque,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::Mutex as StdMutex,
        thread,
    };

    const OLD: &str = "!old:example.org";
    const NEW: &str = "!new:example.org";
    const ME: &str = "@casey:example.org";

    /// A loopback homeserver for one account. It answers the version check,
    /// serves queued sync responses in order (then empty ones), and records
    /// the room of every leave request before answering it with the set
    /// reply, or, while held, never. Each connection has its own thread, so
    /// a held leave does not stop the next one. No real account or network.
    struct LeaveServer {
        url: String,
        leaves: Arc<StdMutex<Vec<String>>>,
        syncs: Arc<StdMutex<VecDeque<Value>>>,
        reply: Arc<StdMutex<(u16, Value)>>,
        hold: Arc<AtomicBool>,
        stopped: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    #[derive(Clone)]
    struct Shared {
        leaves: Arc<StdMutex<Vec<String>>>,
        syncs: Arc<StdMutex<VecDeque<Value>>>,
        reply: Arc<StdMutex<(u16, Value)>>,
        hold: Arc<AtomicBool>,
        stopped: Arc<AtomicBool>,
    }

    impl LeaveServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let shared = Shared {
                leaves: Arc::new(StdMutex::new(Vec::new())),
                syncs: Arc::new(StdMutex::new(VecDeque::new())),
                reply: Arc::new(StdMutex::new((200, json!({})))),
                hold: Arc::new(AtomicBool::new(false)),
                stopped: Arc::new(AtomicBool::new(false)),
            };
            let serving = shared.clone();
            let worker = thread::spawn(move || {
                while !serving.stopped.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let serving = serving.clone();
                            thread::spawn(move || answer(stream, &serving));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                url,
                leaves: shared.leaves,
                syncs: shared.syncs,
                reply: shared.reply,
                hold: shared.hold,
                stopped: shared.stopped,
                worker: Some(worker),
            }
        }

        fn queue_sync(&self, response: Value) {
            self.syncs.lock().unwrap().push_back(response);
        }

        fn answer_leaves(&self, status: u16, body: Value) {
            *self.reply.lock().unwrap() = (status, body);
        }

        /// Leave requests are taken from now on but never answered.
        fn hold_leaves(&self) {
            self.hold.store(true, Ordering::SeqCst);
        }

        fn leaves(&self) -> Vec<String> {
            self.leaves.lock().unwrap().clone()
        }

        async fn wait_for_leaves(&self, count: usize) {
            while self.leaves().len() < count {
                time::sleep(Duration::from_millis(5)).await;
            }
        }
    }

    impl Drop for LeaveServer {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn answer(mut stream: TcpStream, shared: &Shared) {
        stream.set_nonblocking(false).unwrap();
        let timeout = Some(Duration::from_secs(2));
        stream.set_read_timeout(timeout).unwrap();
        let line = read_request(&mut stream);
        let (status, body) = if line.contains("/_matrix/client/versions") {
            (200, json!({"versions": ["v1.15"]}))
        } else if line.contains("/sync") {
            let next = shared.syncs.lock().unwrap().pop_front();
            (200, next.unwrap_or_else(|| json!({"next_batch": "idle"})))
        } else if let Some(room) = leave_target(&line) {
            shared.leaves.lock().unwrap().push(room);
            while shared.hold.load(Ordering::SeqCst) {
                if shared.stopped.load(Ordering::SeqCst) {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            shared.reply.lock().unwrap().clone()
        } else {
            (404, json!({"errcode": "M_UNRECOGNIZED", "error": "x"}))
        };
        let body = body.to_string();
        let response = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
    }

    /// The request line of one HTTP/1.1 request, read with its whole body.
    fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Ok(size) = stream.read(&mut chunk) {
            if size == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..size]);
            let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..end]).to_string();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    let length = name.eq_ignore_ascii_case("content-length");
                    length.then(|| value.trim().parse::<usize>().ok()).flatten()
                })
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                return headers.lines().next().unwrap_or("").to_owned();
            }
        }
        String::new()
    }

    /// The decoded room ID of a leave request line.
    fn leave_target(line: &str) -> Option<String> {
        let path = line.strip_prefix("POST ")?.split(' ').next()?;
        let room = path.strip_prefix("/_matrix/client/v3/rooms/")?;
        let room = room.strip_suffix("/leave")?;
        let mut decoded = Vec::new();
        let mut rest = room.as_bytes();
        while let Some((&first, tail)) = rest.split_first() {
            if first == b'%' && tail.len() >= 2 {
                let hex = std::str::from_utf8(&tail[..2]).ok()?;
                decoded.push(u8::from_str_radix(hex, 16).ok()?);
                rest = &tail[2..];
            } else {
                decoded.push(first);
                rest = tail;
            }
        }
        String::from_utf8(decoded).ok()
    }

    fn state_event(kind: &str, key: &str, content: Value, id: &str) -> Value {
        json!({
            "type": kind,
            "state_key": key,
            "content": content,
            "sender": ME,
            "event_id": id,
            "origin_server_ts": 1,
        })
    }

    /// A first sync: `!old` was replaced by `!new` (its tombstone, and the
    /// predecessor in `!new`'s create event), and the account is joined to
    /// both. `!new`'s name carries a direction override.
    fn joined_both() -> Value {
        let join = json!({"membership": "join"});
        let create = json!({"creator": ME, "room_version": "10"});
        let tombstone = json!({"body": "moved", "replacement_room": NEW});
        let old = [
            state_event("m.room.create", "", create, "$old-create"),
            state_event("m.room.member", ME, join.clone(), "$old-join"),
            state_event("m.room.tombstone", "", tombstone, "$tomb"),
        ];
        let create = json!({
            "creator": ME,
            "room_version": "10",
            "predecessor": {"room_id": OLD, "event_id": "$tomb"},
        });
        let name = json!({"name": "Plans\u{202e} 2026"});
        let new = [
            state_event("m.room.create", "", create, "$new-create"),
            state_event("m.room.member", ME, join, "$new-join"),
            state_event("m.room.name", "", name, "$new-name"),
        ];
        let mut join = Map::new();
        for (room, state) in [(OLD, old), (NEW, new)] {
            let room_update = json!({
                "state": {"events": state},
                "timeline": {"events": [], "limited": false},
            });
            join.insert(room.to_owned(), room_update);
        }
        json!({"next_batch": "s1", "rooms": {"join": join}})
    }

    /// A later sync in which the account has left `room`.
    fn left(room: &str) -> Value {
        let membership = json!({"membership": "leave"});
        let leave = state_event("m.room.member", ME, membership, "$leave");
        let mut rooms = Map::new();
        let room_update = json!({"timeline": {"events": [leave], "limited": false}});
        rooms.insert(room.to_owned(), room_update);
        json!({"next_batch": "s2", "rooms": {"leave": rooms}})
    }

    struct SignedIn {
        directory: std::path::PathBuf,
        manager: Arc<AccountManager>,
        account: crate::AccountInfo,
        selection: AccountSelection,
        client: Client,
    }

    /// The selected account of a fresh manager, signed in with a synthetic
    /// session against `server` and synced once to both rooms.
    async fn signed_in(server: &LeaveServer) -> SignedIn {
        let account = crate::AccountInfo {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: ME.into(),
            device_id: "LEAVE-FIXTURE".into(),
            homeserver: server.url.clone(),
        };
        let (directory, manager, account) = fixture(Some(account)).await;
        let client = {
            let state = manager.state.lock().await;
            state.loaded[&account.id].client.clone()
        };
        let session = matrix_sdk::authentication::matrix::MatrixSession {
            meta: matrix_sdk::SessionMeta {
                user_id: matrix_sdk::ruma::UserId::parse(ME).unwrap(),
                device_id: "LEAVE-FIXTURE".into(),
            },
            tokens: matrix_sdk::SessionTokens {
                access_token: "synthetic access token".into(),
                refresh_token: None,
            },
        };
        let restored = client
            .matrix_auth()
            .restore_session(session, matrix_sdk::store::RoomLoadSettings::default())
            .await;
        restored.unwrap();
        server.queue_sync(joined_both());
        client.sync_once(SyncSettings::default()).await.unwrap();
        let selection = manager.account_selection(&account.id).unwrap();
        SignedIn {
            directory,
            manager,
            account,
            selection,
            client,
        }
    }

    fn room(id: &str) -> OwnedRoomId {
        RoomId::parse(id).unwrap()
    }

    #[test]
    fn one_request_leaves_only_the_reviewed_room_and_nothing_changes_before_sync() {
        runtime().block_on(async {
            let server = LeaveServer::start();
            let SignedIn {
                directory,
                manager,
                selection,
                client,
                ..
            } = signed_in(&server).await;
            let (old, new) = (room(OLD), room(NEW));
            // The fixture: `!new` names `!old` as its predecessor, which is
            // joined too, so `Room::leave` would leave both.
            let replaced = client.get_room(&new).unwrap().predecessor_room();
            assert_eq!(replaced.map(|room| room.room_id), Some(old.clone()));
            assert!(joined(&client, &old) && joined(&client, &new));
            let view = RoomLeaveCancellation::new();
            let review = manager
                .room_leave_review_selected(&selection, NEW, &view)
                .await
                .unwrap();
            assert_eq!(review.room_id, NEW);
            // Other people's text: plain, without the direction override.
            assert_eq!(review.name, "Plans 2026");
            assert!(review.has_predecessor && !review.has_successor);
            // Only the exact room ID: an alias, a link or padding is refused
            // and never resolved.
            for other in [
                "#plans:example.org",
                " !new:example.org",
                "https://matrix.to/#/!new:example.org",
            ] {
                let refused = manager
                    .leave_room_exactly_selected(&selection, other, &view, |_| true)
                    .await;
                assert_eq!(refused.err(), Some(RoomLeaveError::InvalidRoom));
            }
            // A room this device does not hold as joined is refused here.
            let unknown = manager
                .leave_room_exactly_selected(&selection, "!other:example.org", &view, |_| true)
                .await;
            assert_eq!(unknown.err(), Some(RoomLeaveError::NotJoined));
            assert!(server.leaves().is_empty());
            let accepted = manager
                .leave_room_exactly_selected(&selection, NEW, &view, |_| true)
                .await
                .unwrap();
            // One request, for the reviewed room alone.
            assert_eq!(server.leaves(), [NEW]);
            // Accepted, yet nothing changed on this device: both rooms stay
            // joined until sync says otherwise.
            assert!(joined(&client, &new) && joined(&client, &old));
            // The sync that brings the leave is what changes the room.
            server.queue_sync(left(NEW));
            let (left_now, synced) = tokio::join!(
                accepted.left_after_sync(&selection, &view),
                client.sync_once(SyncSettings::default()),
            );
            synced.unwrap();
            assert!(left_now);
            assert!(!joined(&client, &new));
            assert!(joined(&client, &old), "the predecessor was never left");
            assert_eq!(server.leaves(), [NEW]);
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn a_leave_still_waiting_when_the_dialog_closes_or_the_account_changes_is_unknown() {
        runtime().block_on(async {
            let server = LeaveServer::start();
            let SignedIn {
                directory,
                manager,
                account,
                selection,
                client,
            } = signed_in(&server).await;
            server.hold_leaves();
            // The dialog closes while the request is with the homeserver.
            let view = RoomLeaveCancellation::new();
            let leaving = manager.leave_room_exactly_selected(&selection, NEW, &view, |_| true);
            let close = async {
                server.wait_for_leaves(1).await;
                view.cancel();
            };
            let bounded = time::timeout(Duration::from_secs(10), leaving);
            let (result, ()) = tokio::join!(bounded, close);
            assert_eq!(result.unwrap().err(), Some(RoomLeaveError::Unknown));
            // The account changes while the next request is pending.
            let view = RoomLeaveCancellation::new();
            let leaving = manager.leave_room_exactly_selected(&selection, NEW, &view, |_| true);
            let switch = async {
                server.wait_for_leaves(2).await;
                manager.publish_active_account(None);
            };
            let bounded = time::timeout(Duration::from_secs(10), leaving);
            let (result, ()) = tokio::join!(bounded, switch);
            assert_eq!(result.unwrap().err(), Some(RoomLeaveError::Unknown));
            // Neither was sent again, and nothing changed on this device.
            assert_eq!(server.leaves(), [NEW, NEW]);
            assert!(joined(&client, &room(NEW)));
            // The account returns: its old selection stays revoked, so work
            // still holding it is refused before anything is sent.
            manager.publish_active_account(Some(account.clone()));
            let view = RoomLeaveCancellation::new();
            let stale = manager
                .leave_room_exactly_selected(&selection, NEW, &view, |_| true)
                .await;
            assert_eq!(stale.err(), Some(RoomLeaveError::AccountChanged));
            let stale = manager
                .room_leave_review_selected(&selection, NEW, &view)
                .await;
            assert_eq!(stale.err(), Some(RoomLeaveError::AccountChanged));
            assert_eq!(server.leaves().len(), 2);
            // A closed dialog sends nothing at all.
            let current = manager.account_selection(&account.id).unwrap();
            view.cancel();
            let closed = manager
                .leave_room_exactly_selected(&current, NEW, &view, |_| true)
                .await;
            assert_eq!(closed.err(), Some(RoomLeaveError::Cancelled));
            assert_eq!(server.leaves().len(), 2);
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn every_answer_is_final_sent_once_and_changes_nothing_locally() {
        runtime().block_on(async {
            let server = LeaveServer::start();
            let SignedIn {
                directory,
                manager,
                selection,
                client,
                ..
            } = signed_in(&server).await;
            let error = |code: &str| json!({"errcode": code, "error": "homeserver text"});
            // The unknown-token case comes last: it marks the token expired.
            let answers = [
                (429, error("M_LIMIT_EXCEEDED"), RoomLeaveError::RateLimited),
                (403, error("M_FORBIDDEN"), RoomLeaveError::Refused),
                (404, error("M_NOT_FOUND"), RoomLeaveError::Refused),
                (500, error("M_UNKNOWN"), RoomLeaveError::Unknown),
                (502, json!("not json"), RoomLeaveError::Unknown),
                (401, error("M_UNKNOWN_TOKEN"), RoomLeaveError::SignInExpired),
            ];
            for (sent, (status, body, expected)) in answers.into_iter().enumerate() {
                server.answer_leaves(status, body);
                let view = RoomLeaveCancellation::new();
                let result = manager
                    .leave_room_exactly_selected(&selection, NEW, &view, |_| true)
                    .await;
                assert_eq!(result.err(), Some(expected), "status {status}");
                // Exactly one more request: no retry and no replay.
                assert_eq!(server.leaves().len(), sent + 1, "status {status}");
                assert!(joined(&client, &room(NEW)), "status {status}");
                // The text is fixed; the homeserver's never shows.
                assert!(!expected.to_string().contains("homeserver text"));
            }
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn admission_is_rechecked_after_the_real_manager_wait_and_withdrawal_sends_nothing() {
        runtime().block_on(async {
            let server = LeaveServer::start();
            let SignedIn {
                directory,
                manager,
                selection,
                ..
            } = signed_in(&server).await;
            {
                let allowed = AtomicBool::new(true);
                let called = AtomicBool::new(false);
                let hold = manager.state.lock().await;
                let view = RoomLeaveCancellation::new();
                let leaving = manager.leave_room_exactly_selected(&selection, NEW, &view, |_| {
                    called.store(true, Ordering::SeqCst);
                    allowed.load(Ordering::SeqCst)
                });
                tokio::pin!(leaving);
                let pending = std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(leaving.as_mut().poll(cx).is_pending())
                })
                .await;
                assert!(pending && !called.load(Ordering::SeqCst));
                // This is where the app's security/call generation changes while
                // acquisition is queued. No earlier snapshot can admit the send.
                allowed.store(false, Ordering::SeqCst);
                drop(hold);
                assert_eq!(leaving.await.err(), Some(RoomLeaveError::Blocked));
                assert!(called.load(Ordering::SeqCst));
                assert!(server.leaves().is_empty());

                let hold = manager.state.lock().await;
                let view = RoomLeaveCancellation::new();
                let leaving = manager.leave_room_exactly_selected(&selection, NEW, &view, |_| {
                    panic!("a withdrawn dialog must not reach admission")
                });
                tokio::pin!(leaving);
                assert!(
                    std::future::poll_fn(|cx| {
                        std::task::Poll::Ready(leaving.as_mut().poll(cx).is_pending())
                    })
                    .await
                );
                view.cancel();
                assert_eq!(leaving.await.err(), Some(RoomLeaveError::Cancelled));
                drop(hold);
                assert!(server.leaves().is_empty());
            }
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn a_manager_wait_expires_before_admission_without_an_unknown_or_late_send() {
        runtime().block_on(async {
            let server = LeaveServer::start();
            let SignedIn {
                directory,
                manager,
                selection,
                ..
            } = signed_in(&server).await;
            let hold = manager.state.lock().await;
            let view = RoomLeaveCancellation::new();
            let leaving = manager.leave_room_exactly_selected(&selection, NEW, &view, |_| {
                panic!("timed-out acquisition must not reach admission")
            });
            let result = time::timeout(REVIEW_TIMEOUT + Duration::from_secs(2), leaving).await;
            assert_eq!(
                result.unwrap().err(),
                Some(RoomLeaveError::BeforeSendTimeout)
            );
            drop(hold);
            assert!(server.leaves().is_empty());
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn sync_observation_outlives_view_and_selection_but_an_ack_does_not_settle_it() {
        runtime().block_on(async {
            let server = LeaveServer::start();
            let SignedIn {
                directory,
                manager,
                account,
                selection,
                client,
            } = signed_in(&server).await;
            let observation = StdMutex::new(None);
            let view = RoomLeaveCancellation::new();
            let accepted = manager
                .leave_room_exactly_selected(&selection, NEW, &view, |receipt| {
                    *observation.lock().unwrap() = Some(receipt);
                    true
                })
                .await
                .unwrap();
            drop(accepted);
            view.cancel();
            manager.publish_active_account(None);
            manager.publish_active_account(Some(account));
            assert!(!selection.is_current());
            let observation = observation.lock().unwrap().take().unwrap();
            let weak_owner = observation.owner.clone();
            let lifetime = RoomLeaveCancellation::new();
            let left_from_sync = observation.left_from_sync(&lifetime);
            tokio::pin!(left_from_sync);
            // An acknowledgment, view close and A-B-A are not sync evidence.
            assert!(
                std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(left_from_sync.as_mut().poll(cx).is_pending())
                })
                .await
            );
            server.queue_sync(left(NEW));
            let (observed, synced) = tokio::join!(
                time::timeout(Duration::from_secs(2), left_from_sync),
                client.sync_once(SyncSettings::default()),
            );
            synced.unwrap();
            assert!(observed.unwrap());
            assert_eq!(server.leaves(), [NEW]);
            drop(client);
            drop(manager);
            assert!(
                weak_owner.upgrade().is_none(),
                "an observation cannot pin its loaded account"
            );
            let _ = std::fs::remove_dir_all(directory);
        });
    }
}
