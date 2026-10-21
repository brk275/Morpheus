//! Room and Space invitations of one account selection.
//!
//! A listing reads only the invite state the SDK stored from sync: no request
//! is sent, and every summary field is what that state supplied, or absent.
//! An answer is one join (accept) or leave (decline) request for exactly one
//! room the account is invited to now, bound to the caller's
//! `AccountSelection` and a 30 s bound. Errors use fixed wording and never
//! pass homeserver text on.
//!
//! What an answer reports is only what is known:
//! - `Err`: nothing was applied. Nothing was sent (the room is no longer an
//!   invitation, the selection or the caller's fence ended first, the invite
//!   state could not be read, the request could not be built, data it needed
//!   first could not be fetched, or its connection never opened), or the
//!   homeserver refused the request (an answer below 500).
//! - `Unconfirmed`: the request was handed to the homeserver and no answer
//!   came back (none, a server error, the bound, or a selection change while
//!   it was out). It may have applied, and cannot be recalled.
//! - `Confirmed`: the homeserver confirmed it; this device's store does not
//!   show the new membership yet.
//! - `Settled`: confirmed, and the account's sync then brought exactly the
//!   membership the answer asked for (joined after an accept, left after a
//!   decline) into the local store. A room missing from the store, or in any
//!   other state, is not settled.
//!
//! Besides the exact selection, the caller's own `AnswerFence` (its view,
//! a security step, the app still running) is asked inside the race, right
//! before the request is handed over; a `false` or a selection that ended
//! sends nothing and is an `Err`. The Space kind is taken from the invite
//! state when the answer is admitted.
//!
//! Every wait before the hand-off (the manager's client lookup and the
//! invite-state reads) is bounded and withdrawn as soon as the fence closes
//! (asked every `FENCE_POLL`, the fence first): nothing is sent then. Right
//! before the hand-off, after those waits, the room must still be an
//! invitation in the store; a sync that withdrew, declined or joined it
//! meanwhile sends nothing (`RoomNotFound`). A join counts as confirmed only
//! when its answer names exactly the room asked for; an answer naming any
//! other room is `Unconfirmed`: it may have done something, and it is never
//! listed or opened.
//!
//! A confirmed join stays confirmed, whatever happens next. After it, a
//! direct-message invitation is listed under its inviter in the account's
//! `m.direct` data through the shared lossless writer
//! (`direct_account_data::list_direct_room`): the stored content is merged as
//! raw JSON, so every other user, unknown field and number keeps its exact
//! text, odd content is refused rather than rewritten, a fresh read right
//! before the single write must match the first byte for byte or nothing is
//! written, and nothing is ever retried. Each of its requests needs the exact
//! selection, the fence's `may_list` and its own 30 s bound; it runs
//! alongside the sync wait, and the shared writer serializes it with every
//! other `m.direct` listing in this process (People's included). Its
//! outcome is reported as `DirectMark`; an unconfirmed join is never listed,
//! then or later. Matrix account data has no conditional write: a change
//! made elsewhere between that re-read and the write can still be
//! overwritten. `direct_account_data.rs` is the one shared writer; its
//! process-wide turn (`direct_turn`) serializes every listing here.
//!
//! The requests are typed `POST /rooms/{roomId}/join` and
//! `POST /rooms/{roomId}/leave` for that room alone, sent on the account's
//! client with a bounded request config. They never go through
//! `Room::leave`, which also leaves every joined or invited predecessor room
//! it finds (an invitation can point to a room already joined) and walks
//! the cached predecessors without a bound. The account's sync then brings
//! the new membership into the local store. Each request is sent at most
//! once: no HTTP retry (`retry_limit(0)`, which alone does not stop the SDK
//! from refreshing an expired access token and sending the request again)
//! and no automatic authentication refresh and replay
//! (`disable_auth_refresh`), with a 30 s timeout and a 64 KiB response cap
//! (`max_response_bytes`), on the client's own no-redirect, TLS 1.2+
//! transport. An expired token is then a refusal that needs a new
//! deliberate attempt. Both options are the vendored SDK's
//! (`RequestConfig`).

use std::{
    future::Future,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use matrix_sdk::{
    Client, HttpError, Room, RoomState,
    config::RequestConfig,
    ruma::{
        OwnedUserId, RoomId, UserId,
        api::client::membership::{join_room_by_id, leave_room},
    },
    sync::RoomUpdates,
};
use tokio::sync::broadcast;

use super::*;
use crate::{
    IgnoredUsers,
    direct_account_data::{DirectListing, DirectWriteError, DirectWriteGuard, list_direct_room},
    notification_rules::refused,
};

/// Most invitations described by one listing: the first ones by name.
pub const MAX_INVITATIONS: usize = 500;
/// Untrusted names, topics and IDs are cut to this many characters.
pub const MAX_INVITATION_TEXT_CHARS: usize = 256;
/// Bound on one answer request.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait for the account's sync to bring an answered invitation's new
/// membership into the local store.
const MEMBERSHIP_SYNC_WAIT: Duration = Duration::from_secs(30);
const NOT_APPLIED_ACCEPT: &str = "accepting the invitation was refused or not sent";
const NOT_APPLIED_DECLINE: &str = "declining the invitation was refused or not sent";
const UNCONFIRMED_ACCEPT: &str = "accepting the invitation was not confirmed";
const UNCONFIRMED_DECLINE: &str = "declining the invitation was not confirmed";
const WITHDRAWN_ACCEPT: &str = "accepting the invitation was withdrawn before it was sent";
const WITHDRAWN_DECLINE: &str = "declining the invitation was withdrawn before it was sent";
/// Bound on listing an accepted direct-message invitation in `m.direct`:
/// its turn, its reads and its one write together.
const DIRECT_LISTING_BOUND: Duration = Duration::from_secs(30);
/// How often a listing or an answer waiting before its hand-off asks the
/// caller's fence again.
const FENCE_POLL: Duration = Duration::from_millis(100);
/// Bound on an answer's waits before its hand-off: the manager's client
/// lookup and the invite-state reads, together.
const ANSWER_PREFLIGHT_BOUND: Duration = Duration::from_secs(10);

/// The caller's own conditions on one answer, beyond its exact account
/// selection, which the core checks itself. Each is asked right before a
/// request is handed over, and a `false` sends nothing more; a request
/// already handed over is not recalled.
pub trait AnswerFence: Sync {
    /// Whether the join or leave may still be handed over.
    fn may_answer(&self) -> bool;
    /// Whether the `m.direct` listing after a confirmed join may still send
    /// its next request.
    fn may_list(&self) -> bool;
}

/// No condition beyond the selection: the unbound paths and tests.
struct Unfenced;

impl AnswerFence for Unfenced {
    fn may_answer(&self) -> bool {
        true
    }

    fn may_list(&self) -> bool {
        true
    }
}

/// What became of listing an accepted direct-message invitation under its
/// inviter in the account's `m.direct` data. Nothing here is retried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectMark {
    /// Not an accepted direct-message invitation: there is nothing to list.
    NotDirect,
    /// Not tried, now or later: the join was not confirmed, or the invite
    /// state did not name the inviter. Nothing was written.
    NotTried,
    /// Listed by the one write, or already listed in the fresh read.
    Listed(DirectListing),
    /// Not listed, and nothing written; or, for `Unknown`, the write got no
    /// answer and may have applied.
    Failed(DirectWriteError),
}

/// Where a confirmed or possibly applied answer stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvitationOutcome {
    /// Confirmed, and this device's store has the membership it asked for.
    Settled,
    /// Confirmed; the store still reads the room as an invitation.
    Confirmed,
    /// Handed to the homeserver with no answer back: it may have applied.
    Unconfirmed,
}

/// An answer that reached the point of being sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvitationAnswer {
    pub outcome: InvitationOutcome,
    /// The invitation was to a Space, as its invite state said when the
    /// answer was admitted.
    pub is_space: bool,
    /// The `m.direct` listing of an accepted direct-message invitation.
    pub direct: DirectMark,
}

/// The membership an answer asks for.
fn asked_for(accept: bool) -> RoomState {
    if accept {
        RoomState::Joined
    } else {
        RoomState::Left
    }
}

/// Whether this device's store has `room_id` in the membership the answer
/// asked for: joined after an accept, left after a decline. A room it does
/// not have, or has in any other state, is not settled.
fn settled(client: &Client, room_id: &RoomId, accept: bool) -> bool {
    client
        .get_room(room_id)
        .is_some_and(|room| room.state() == asked_for(accept))
}

/// Whether `room_id` reached the membership the answer asked for in the
/// local store, waiting at most `bound` for the account's sync, with
/// `updates` subscribed before the answer was sent so its sync cannot be
/// missed.
async fn membership_synced(
    client: &Client,
    room_id: &RoomId,
    accept: bool,
    updates: &mut broadcast::Receiver<RoomUpdates>,
    bound: Duration,
) -> bool {
    let waited = tokio::time::timeout(bound, async {
        while !settled(client, room_id, accept) {
            if let Err(broadcast::error::RecvError::Closed) = updates.recv().await {
                return false;
            }
        }
        true
    });
    waited.await.unwrap_or(false)
}

/// Most bytes accepted in one answer response (they carry `{}`, a room ID or
/// an error).
const MAX_ANSWER_RESPONSE_BYTES: u64 = 64 * 1024;

/// The request config of every answer (leave, join): sent at most once,
/// with no HTTP retry and no automatic authentication refresh and replay,
/// in a bounded time and response size. The `m.direct` listing uses the
/// shared writer's own configs, which are single bounded attempts too.
fn answer_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .disable_auth_refresh()
        .max_response_bytes(MAX_ANSWER_RESPONSE_BYTES)
        .timeout(ANSWER_TIMEOUT)
}

/// Why one answer request got no confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unanswered {
    /// Not applied: the homeserver refused it (an answer below 500), or it
    /// never left (the request could not be built, data it needed first could
    /// not be fetched, or no connection opened).
    NotApplied,
    /// No usable answer once it may have left (none, a server error, an
    /// unreadable response): it may have applied.
    Unknown,
    /// Never handed over: the selection or the caller's fence ended first.
    Withdrawn,
    /// Never handed over: the room was no longer an invitation in the store
    /// right before the hand-off.
    Gone,
}

type Answered = std::result::Result<(), Unanswered>;

fn unanswered(error: &HttpError) -> Unanswered {
    let not_sent = match error {
        HttpError::IntoHttp(_) => true,
        HttpError::Reqwest(error) => error.is_builder() || error.is_connect(),
        // Cached data the request needed first (such as the server's
        // versions) could not be fetched: the request did not leave.
        HttpError::Cached(_) => true,
        _ => false,
    };
    let refused = error
        .as_client_api_error()
        .is_some_and(|answer| answer.status_code.is_client_error());
    if not_sent || refused {
        Unanswered::NotApplied
    } else {
        Unanswered::Unknown
    }
}

/// Leave exactly `room_id`: one typed leave request for that room alone. No
/// predecessor is visited or left.
async fn leave_exactly(client: &Client, room_id: &RoomId) -> Answered {
    let request = leave_room::v3::Request::new(room_id.to_owned());
    let sent = client
        .send(request)
        .with_request_config(answer_config())
        .await;
    sent.map(|_| ()).map_err(|error| unanswered(&error))
}

/// Join exactly `room_id`: one typed join request for that room alone. It
/// is confirmed only when the answer names exactly `room_id`
/// (`joined_exactly`).
async fn join_exactly(client: &Client, room_id: &RoomId) -> Answered {
    let request = join_room_by_id::v3::Request::new(room_id.to_owned());
    let sent = client
        .send(request)
        .with_request_config(answer_config())
        .await;
    match sent {
        Ok(response) => joined_exactly(room_id, &response.room_id),
        Err(error) => Err(unanswered(&error)),
    }
}

/// A join answer confirms the join asked for only when it names exactly
/// `asked`. Any other room, however valid, is no confirmation: the request
/// was handed over and may have done something (`Unknown`).
fn joined_exactly(asked: &RoomId, answered: &RoomId) -> Answered {
    if asked.as_str() == answered.as_str() {
        Ok(())
    } else {
        Err(Unanswered::Unknown)
    }
}

/// Whether an invitation is a direct message, and from whom, as its invite
/// state says.
enum DirectInvite {
    /// Not a direct-message invitation.
    No,
    /// A direct-message invitation whose invite state names no inviter.
    Unnamed,
    From(OwnedUserId),
}

impl DirectInvite {
    /// The listing outcome of an answer that never reached the listing.
    fn untried(&self) -> DirectMark {
        match self {
            DirectInvite::No => DirectMark::NotDirect,
            DirectInvite::Unnamed | DirectInvite::From(_) => DirectMark::NotTried,
        }
    }
}

/// Whether `room` is a direct-message invitation, and its inviter, from its
/// invite state.
async fn direct_inviter(room: &Room) -> Result<DirectInvite> {
    if !room.is_direct().await.unwrap_or(false) {
        return Ok(DirectInvite::No);
    }
    Ok(match room.invite_details().await {
        Ok(details) => DirectInvite::From(details.inviter_id),
        Err(_) => DirectInvite::Unnamed,
    })
}

/// Ends one `m.direct` listing: the answer's exact selection (`None` on the
/// unbound path), the caller's fence and the listing's own bound.
struct ListingGuard<'a, F> {
    selection: Option<&'a AccountSelection>,
    fence: &'a F,
    deadline: tokio::time::Instant,
}

impl<F: AnswerFence> DirectWriteGuard for ListingGuard<'_, F> {
    fn stopped(&self) -> bool {
        self.selection
            .is_some_and(|selection| !selection.is_current())
            || !self.fence.may_list()
            || tokio::time::Instant::now() >= self.deadline
    }

    fn wait_stopped(&self) -> impl Future<Output = ()> + Send {
        async move {
            // The selection and the bound end it at once; the fence is asked
            // again every `FENCE_POLL`.
            let revoked = async {
                match self.selection {
                    Some(selection) => selection.revoked().await,
                    None => std::future::pending().await,
                }
            };
            let polled = async {
                while !self.stopped() {
                    tokio::time::sleep(FENCE_POLL).await;
                }
            };
            tokio::select! {
                () = revoked => {}
                () = tokio::time::sleep_until(self.deadline) => {}
                () = polled => {}
            }
        }
    }
}

/// List `room_id` under `inviter` in the client's own `m.direct` data with
/// the shared lossless writer (see the module notes). The writer itself
/// takes the process-wide `m.direct` turn, shared with every other writer
/// (People's included), so none is taken here; a guard that ends while
/// waiting for it sends nothing.
async fn mark_direct(
    client: &Client,
    guard: &ListingGuard<'_, impl AnswerFence>,
    room_id: &RoomId,
    inviter: &UserId,
) -> DirectMark {
    let Some(own) = client.user_id() else {
        return DirectMark::Failed(DirectWriteError::NotSent);
    };
    if guard.stopped() {
        return DirectMark::Failed(DirectWriteError::NotSent);
    }
    match list_direct_room(client, own, inviter, room_id, guard).await {
        Ok(listed) => DirectMark::Listed(listed),
        Err(error) => DirectMark::Failed(error),
    }
}

/// An answer's wait before its hand-off, bounded by `bound` and withdrawn as
/// soon as `fence` closes (asked first, then every `FENCE_POLL`). Neither
/// sends anything: a closed fence is the withdrawn refusal, the bound is
/// `AccountAccessBusy`.
async fn before_hand_off<T>(
    fence: &impl AnswerFence,
    bound: Duration,
    withdrawn: &'static str,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    let closed = async {
        while fence.may_answer() {
            tokio::time::sleep(FENCE_POLL).await;
        }
    };
    tokio::select! {
        biased;
        () = closed => Err(MatrixCoreError::Sdk(withdrawn.into())),
        waited = tokio::time::timeout(bound, work) => {
            waited.unwrap_or(Err(MatrixCoreError::AccountAccessBusy))
        }
    }
}

/// Whether the store still has `room_id` as an invitation, right before the
/// hand-off: a sync may have withdrawn, declined or joined it meanwhile.
fn still_invited(client: &Client, room_id: &RoomId) -> bool {
    client
        .get_room(room_id)
        .is_some_and(|room| room.state() == RoomState::Invited)
}

/// Test only: an answer of the account named in `HELD_PREFLIGHTS` waits
/// right after its preflight until the test lets it go, so the test can
/// change the room meanwhile. Nothing else is held.
#[cfg(test)]
type PreflightHold = (
    String,
    std::sync::Arc<tokio::sync::Notify>,
    std::sync::Arc<tokio::sync::Notify>,
);
#[cfg(test)]
static HELD_PREFLIGHTS: std::sync::Mutex<Vec<PreflightHold>> = std::sync::Mutex::new(Vec::new());
#[cfg(test)]
async fn held_preflight(account: &str) {
    let hold = {
        let mut holds = HELD_PREFLIGHTS.lock().unwrap();
        let at = holds.iter().position(|(held, ..)| held == account);
        at.map(|at| holds.remove(at))
    };
    if let Some((_, entered, proceed)) = hold {
        entered.notify_one();
        proceed.notified().await;
    }
}

/// The unbound accept (`AccountManager::accept_invite`): the same single
/// bounded join, then the same lossless, bounded `m.direct` listing, whose
/// outcome this path does not report. Any failure of the join reads as
/// unconfirmed; this path does not tell a refusal from a lost answer.
pub(crate) async fn join_room_exactly(client: &Client, room: &Room) -> Result<()> {
    let direct = direct_inviter(room).await?;
    // Still an invitation after the invite-state reads, or nothing is sent.
    if !still_invited(client, room.room_id()) {
        return Err(MatrixCoreError::RoomNotFound(room.room_id().to_string()));
    }
    join_exactly(client, room.room_id())
        .await
        .map_err(refused(UNCONFIRMED_ACCEPT))?;
    if let DirectInvite::From(inviter) = direct {
        let guard = ListingGuard {
            selection: None,
            fence: &Unfenced,
            deadline: tokio::time::Instant::now() + DIRECT_LISTING_BOUND,
        };
        let _ = mark_direct(client, &guard, room.room_id(), &inviter).await;
    }
    Ok(())
}

/// The unbound decline (`AccountManager::reject_invite`): the same single
/// bounded leave of exactly `room_id`.
pub(crate) async fn leave_room_exactly(client: &Client, room_id: &RoomId) -> Result<()> {
    leave_exactly(client, room_id)
        .await
        .map_err(refused(UNCONFIRMED_DECLINE))
}

/// One invitation as its invite state describes it. Every text field is
/// untrusted and bounded; an absent field was not supplied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvitationSummary {
    pub room_id: String,
    /// The SDK's display name for the room, computed from the invite state.
    pub name: String,
    pub topic: Option<String>,
    pub canonical_alias: Option<String>,
    pub is_space: bool,
    pub is_direct: bool,
    /// The sender of this account's invite membership event.
    pub inviter_id: Option<String>,
    /// That sender's display name, only when its own member event was
    /// supplied with one.
    pub inviter_name: Option<String>,
    /// That sender is on the account's ignore list (`m.ignored_user_list`),
    /// read with the strict reader; `false` also when the list could not be
    /// read, which leaves the invitation unmarked, never marked as allowed.
    pub inviter_ignored: bool,
    /// `Some` only when the invite state says whether the room is encrypted.
    pub encrypted: Option<bool>,
}

/// A bounded listing: at most `MAX_INVITATIONS` summaries, sorted by name,
/// and how many invitations the account has in all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Invitations {
    pub invitations: Vec<InvitationSummary>,
    pub total: usize,
}

fn bounded(text: &str) -> String {
    text.chars().take(MAX_INVITATION_TEXT_CHARS).collect()
}

/// What is shown: a text that is present and not blank.
fn supplied(text: Option<&str>) -> Option<String> {
    text.map(str::trim)
        .filter(|text| !text.is_empty())
        .map(bounded)
}

async fn summary(room: &Room, ignored: Option<&IgnoredUsers>) -> InvitationSummary {
    let room_id = room.room_id().to_string();
    let name = match room.display_name().await {
        Ok(name) => bounded(&name.to_string()),
        Err(_) => String::new(),
    };
    // The invite member event and its sender, from the store only.
    let details = room.invite_details().await.ok();
    let inviter_ignored = match (&details, ignored) {
        (Some(details), Some(ignored)) => ignored.contains(&details.inviter_id),
        _ => false,
    };
    let encryption = room.encryption_state();
    InvitationSummary {
        name,
        topic: supplied(room.topic().as_deref()),
        canonical_alias: room.canonical_alias().map(|alias| bounded(alias.as_str())),
        is_space: room.is_space(),
        is_direct: room.is_direct().await.unwrap_or(false),
        inviter_id: details
            .as_ref()
            .map(|details| bounded(details.inviter_id.as_str())),
        inviter_name: details
            .as_ref()
            .and_then(|details| details.inviter.as_ref())
            .and_then(|inviter| supplied(inviter.display_name())),
        inviter_ignored,
        encrypted: (!encryption.is_unknown()).then_some(encryption.is_encrypted()),
        room_id,
    }
}

impl AccountManager {
    /// The selected account's invitations as the SDK holds them from sync,
    /// sorted by name and bounded by `MAX_INVITATIONS`: past the bound, the
    /// first rooms by name are described. No request is sent.
    pub async fn invitations_selected(&self, selection: &AccountSelection) -> Result<Invitations> {
        let client = self.rule_client(selection).await?;
        // One strict read of the ignore list per listing; an unreadable list
        // leaves every invitation unmarked. No lock is held here.
        let ignored = self.cached_ignored_users_selected(selection).await.ok();
        self.under_rule_selection(selection, async move {
            let mut rooms = client
                .invited_rooms()
                .into_iter()
                .map(|room| {
                    let name = match room.cached_display_name() {
                        Some(name) => name.to_string(),
                        None => room.room_id().to_string(),
                    };
                    (name.to_lowercase(), room)
                })
                .collect::<Vec<_>>();
            let total = rooms.len();
            rooms.sort_by(|a, b| (&a.0, a.1.room_id()).cmp(&(&b.0, b.1.room_id())));
            rooms.truncate(MAX_INVITATIONS);
            let mut invitations = Vec::with_capacity(rooms.len());
            for (_, room) in &rooms {
                invitations.push(summary(room, ignored.as_ref()).await);
            }
            Ok(Invitations { invitations, total })
        })
        .await
    }

    /// Accept (join) or decline (leave) exactly one invitation of the
    /// selected account, never another room. Without a request, it fails
    /// with `RoomNotFound` when `room_id` is not a room that account is
    /// invited to now, and with `SelectedAccountChanged` or a withdrawn
    /// refusal when the selection or `fence` ended before the hand-off.
    /// `Err` means nothing was applied; `Ok` says whether the homeserver
    /// confirmed the answer, whether the account's sync then brought the
    /// asked-for membership here within 30 s, and what became of the
    /// `m.direct` listing (see the module notes). An unconfirmed or unsettled
    /// answer, and an unknown listing, must not be sent again.
    pub async fn respond_invitation_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        accept: bool,
        fence: &impl AnswerFence,
    ) -> Result<InvitationAnswer> {
        let bounds = (
            MEMBERSHIP_SYNC_WAIT,
            DIRECT_LISTING_BOUND,
            ANSWER_PREFLIGHT_BOUND,
        );
        self.answer_invitation(selection, room_id, accept, fence, bounds)
            .await
    }

    /// `respond_invitation_selected` with its sync wait, listing bound and
    /// preflight bound.
    async fn answer_invitation(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        accept: bool,
        fence: &impl AnswerFence,
        (sync_wait, listing_bound, preflight_bound): (Duration, Duration, Duration),
    ) -> Result<InvitationAnswer> {
        let id =
            RoomId::parse(room_id).map_err(|e| MatrixCoreError::InvalidRoomId(e.to_string()))?;
        let withdrawn = if accept {
            WITHDRAWN_ACCEPT
        } else {
            WITHDRAWN_DECLINE
        };
        // Everything before the hand-off, bounded and withdrawn by the fence:
        // the manager's client lookup and the invite-state reads.
        let preflight = async {
            let client = self.rule_client(selection).await?;
            let room = client
                .get_room(&id)
                .filter(|room| room.state() == RoomState::Invited)
                .ok_or_else(|| MatrixCoreError::RoomNotFound(room_id.into()))?;
            // Read from the invite state now: after the answer the room may
            // read differently here, or not at all.
            let is_space = room.is_space();
            let direct = if accept {
                self.under_rule_selection(selection, direct_inviter(&room))
                    .await?
            } else {
                DirectInvite::No
            };
            Ok((client, is_space, direct))
        };
        let (client, is_space, direct) =
            before_hand_off(fence, preflight_bound, withdrawn, preflight).await?;
        #[cfg(test)]
        held_preflight(selection.account_id()).await;
        // Subscribed before the request, so the sync bringing its membership
        // cannot be missed.
        let mut updates = client.subscribe_to_all_room_updates();
        // Set right before the request goes to the client: until then,
        // nothing was sent.
        let handed = AtomicBool::new(false);
        let request = async {
            // The last checks, inside the race and after every wait: nothing
            // leaves once the selection or the caller's fence ended, or once
            // the room is no longer an invitation here.
            if !selection.is_current() || !fence.may_answer() {
                return Err(Unanswered::Withdrawn);
            }
            if !still_invited(&client, &id) {
                return Err(Unanswered::Gone);
            }
            handed.store(true, Ordering::Release);
            if accept {
                join_exactly(&client, &id).await
            } else {
                leave_exactly(&client, &id).await
            }
        };
        // From the hand-off the request may reach the homeserver: a
        // selection change or the bound only loses its answer.
        let sent = selection
            .guard(tokio::time::timeout(ANSWER_TIMEOUT, request))
            .await;
        let refusal = if accept {
            NOT_APPLIED_ACCEPT
        } else {
            NOT_APPLIED_DECLINE
        };
        match sent {
            Some(Ok(Ok(()))) => {}
            Some(Ok(Err(Unanswered::NotApplied))) => {
                return Err(MatrixCoreError::Sdk(refusal.into()));
            }
            Some(Ok(Err(Unanswered::Withdrawn))) => {
                return Err(MatrixCoreError::Sdk(withdrawn.into()));
            }
            // A sync changed the room before the hand-off: nothing was sent.
            Some(Ok(Err(Unanswered::Gone))) => {
                return Err(MatrixCoreError::RoomNotFound(room_id.into()));
            }
            // The selection ended before the hand-off: nothing was sent.
            None if !handed.load(Ordering::Acquire) => {
                return Err(MatrixCoreError::SelectedAccountChanged);
            }
            _ => {
                return Ok(InvitationAnswer {
                    outcome: InvitationOutcome::Unconfirmed,
                    is_space,
                    direct: direct.untried(),
                });
            }
        }
        // Confirmed. Nothing below makes it less so. The listing runs
        // alongside the sync wait, so it adds no wait beyond the longer one.
        let listing = async {
            let DirectInvite::From(inviter) = &direct else {
                return direct.untried();
            };
            let guard = ListingGuard {
                selection: Some(selection),
                fence,
                deadline: tokio::time::Instant::now() + listing_bound,
            };
            mark_direct(&client, &guard, &id, inviter).await
        };
        let synced = async {
            let synced = membership_synced(&client, &id, accept, &mut updates, sync_wait);
            selection.guard(synced).await.unwrap_or(false)
        };
        let (listed, synced) = tokio::join!(listing, synced);
        Ok(InvitationAnswer {
            outcome: if synced {
                InvitationOutcome::Settled
            } else {
                InvitationOutcome::Confirmed
            },
            is_space,
            direct: listed,
        })
    }

    /// For each answered invitation, as (room ID, accepted), whether this
    /// device's store now has the membership the answer asked for: joined
    /// after an accept, left after a decline. No request is sent; only the
    /// first `MAX_INVITATIONS` are checked, and the rest are left out.
    pub async fn invitation_answers_settled_selected(
        &self,
        selection: &AccountSelection,
        answers: &[(String, bool)],
    ) -> Result<Vec<bool>> {
        let client = self.rule_client(selection).await?;
        self.under_rule_selection(selection, async move {
            let checked: Vec<bool> = answers
                .iter()
                .take(MAX_INVITATIONS)
                .map(|(room_id, accept)| {
                    RoomId::parse(room_id).is_ok_and(|id| settled(&client, &id, *accept))
                })
                .collect();
            Ok(checked)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::{
        SessionMeta,
        authentication::{SessionTokens, matrix::MatrixSession},
        config::SyncSettings,
        ruma::{owned_device_id, owned_user_id, room_id},
    };
    use std::{
        io::{BufRead, BufReader, Read, Write},
        sync::{Arc, Mutex},
    };

    /// Joined room A, left room L, an invitation to B whose create event
    /// names A as its predecessor (an upgraded room's invitation), an
    /// invitation to the Space S, and a direct-message invitation D from
    /// @dana.
    const SYNC: &str = r#"{
        "next_batch": "s1",
        "rooms": {
            "join": {
                "!a:example.org": {
                    "state": {"events": [
                        {"type": "m.room.create", "state_key": "", "event_id": "$a1",
                         "sender": "@bob:example.org", "origin_server_ts": 1,
                         "content": {"creator": "@bob:example.org", "room_version": "10"}},
                        {"type": "m.room.member", "state_key": "@alice:example.org",
                         "event_id": "$a2", "sender": "@alice:example.org",
                         "origin_server_ts": 2, "content": {"membership": "join"}}
                    ]},
                    "timeline": {"events": [], "limited": false}
                }
            },
            "leave": {
                "!l:example.org": {
                    "state": {"events": [
                        {"type": "m.room.create", "state_key": "", "event_id": "$l1",
                         "sender": "@bob:example.org", "origin_server_ts": 3,
                         "content": {"creator": "@bob:example.org", "room_version": "10"}},
                        {"type": "m.room.member", "state_key": "@alice:example.org",
                         "event_id": "$l2", "sender": "@alice:example.org",
                         "origin_server_ts": 4, "content": {"membership": "leave"}}
                    ]},
                    "timeline": {"events": [], "limited": false}
                }
            },
            "invite": {
                "!b:example.org": {
                    "invite_state": {"events": [
                        {"type": "m.room.create", "state_key": "", "sender": "@bob:example.org",
                         "content": {"creator": "@bob:example.org", "room_version": "10",
                                     "predecessor": {"room_id": "!a:example.org",
                                                     "event_id": "$old"}}},
                        {"type": "m.room.member", "state_key": "@alice:example.org",
                         "sender": "@bob:example.org", "content": {"membership": "invite"}}
                    ]}
                },
                "!s:example.org": {
                    "invite_state": {"events": [
                        {"type": "m.room.create", "state_key": "", "sender": "@bob:example.org",
                         "content": {"creator": "@bob:example.org", "room_version": "10",
                                     "type": "m.space"}},
                        {"type": "m.room.member", "state_key": "@alice:example.org",
                         "sender": "@bob:example.org", "content": {"membership": "invite"}}
                    ]}
                },
                "!d:example.org": {
                    "invite_state": {"events": [
                        {"type": "m.room.create", "state_key": "", "sender": "@dana:example.org",
                         "content": {"creator": "@dana:example.org", "room_version": "10"}},
                        {"type": "m.room.member", "state_key": "@alice:example.org",
                         "sender": "@dana:example.org",
                         "content": {"membership": "invite", "is_direct": true}}
                    ]}
                }
            }
        }
    }"#;

    fn reply(path: &str) -> &'static str {
        if path.starts_with("/_matrix/client/versions") {
            r#"{"versions": ["v1.1", "v1.2", "v1.3", "v1.4", "v1.5", "v1.6", "v1.7", "v1.8"]}"#
        } else if path.starts_with("/_matrix/client/v3/sync") {
            SYNC
        } else if path.contains("/keys/upload") {
            r#"{"one_time_key_counts": {"signed_curve25519": 50}}"#
        } else if path.contains("/keys/query") {
            r#"{"device_keys": {}, "failures": {}}"#
        } else {
            "{}"
        }
    }

    /// What a loopback homeserver saw: `METHOD path`, or `METHOD path body`.
    type Seen = Arc<Mutex<Vec<String>>>;

    /// A loopback homeserver stand-in that records `METHOD path` for every
    /// request and answers one request per connection; a leave is answered
    /// with `leave_status` (an error body unless it is 200).
    fn fake_homeserver(leave_status: u16) -> (String, Seen) {
        let (address, seen, _) = loopback(move |_, path| {
            if path.ends_with("/leave") && leave_status != 200 {
                (leave_status, ERROR.to_owned())
            } else {
                (200, reply(path).to_owned())
            }
        });
        (address, seen)
    }

    const ERROR: &str = r#"{"errcode": "M_UNKNOWN", "error": "x"}"#;

    /// A loopback homeserver whose `m.direct` reads answer `reads` in turn,
    /// the last one again once the others ran out (`None` is "not found"),
    /// whose writes are accepted after `write_delay`, and whose joins answer
    /// `join_status`.
    fn direct_homeserver(
        reads: &[Option<&str>],
        write_delay: Duration,
        join_status: u16,
    ) -> (String, Seen, Seen) {
        let mut reads = reads
            .iter()
            .map(|read| read.map(str::to_owned))
            .collect::<std::collections::VecDeque<_>>();
        loopback(move |method, path| {
            if path.contains("/account_data/m.direct") {
                if method == "PUT" {
                    std::thread::sleep(write_delay);
                    return (200, "{}".to_owned());
                }
                let read = if reads.len() > 1 {
                    reads.pop_front()
                } else {
                    reads.front().cloned()
                };
                return match read.flatten() {
                    Some(stored) => (200, stored),
                    None => (
                        404,
                        r#"{"errcode": "M_NOT_FOUND", "error": "x"}"#.to_owned(),
                    ),
                };
            }
            match (path.ends_with("/join"), join_status) {
                (true, 200) => (200, r#"{"room_id": "!d:example.org"}"#.to_owned()),
                (true, status) => (status, ERROR.to_owned()),
                (false, _) => (200, reply(path).to_owned()),
            }
        })
    }

    /// The loopback itself: it records `METHOD path` for every request in
    /// the first log and `METHOD path body` in the second, and answers each
    /// with `respond(method, path)`, one request per connection.
    fn loopback(
        mut respond: impl FnMut(&str, &str) -> (u16, String) + Send + 'static,
    ) -> (String, Seen, Seen) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let (seen, bodies) = (Seen::default(), Seen::default());
        let (log, body_log) = (seen.clone(), bodies.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                let Some((method, path, body)) = read_request(&stream) else {
                    continue;
                };
                let request = format!("{method} {path}");
                body_log.lock().unwrap().push(format!("{request} {body}"));
                log.lock().unwrap().push(request);
                let (status, answer) = respond(method.as_str(), path.as_str());
                write_answer(&mut stream, status, &answer);
            }
        });
        (address, seen, bodies)
    }

    /// One request read off `stream`: its method, path and body.
    fn read_request(stream: &std::net::TcpStream) -> Option<(String, String, String)> {
        let mut reader = BufReader::new(stream.try_clone().ok()?);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let path = parts.next().unwrap_or_default().to_owned();
        let mut length = 0;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                break;
            }
            let header = header.to_ascii_lowercase();
            if let Some(value) = header.strip_prefix("content-length:") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0; length];
        let _ = reader.read_exact(&mut body);
        Some((method, path, String::from_utf8_lossy(&body).into_owned()))
    }

    fn write_answer(stream: &mut std::net::TcpStream, status: u16, answer: &str) {
        let _ = write!(
            stream,
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{answer}",
            answer.len()
        );
    }

    /// A loopback homeserver keeping one `m.direct` content, each connection
    /// on its own thread so one writer's held write does not hold another's
    /// reads: a read returns what is stored, a write stores its body, and the
    /// first write waits until `release` is set. It records `METHOD path`.
    fn direct_store_homeserver() -> (String, Seen, Arc<Mutex<String>>, Arc<AtomicBool>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let seen = Seen::default();
        let stored = Arc::new(Mutex::new(STORED_DIRECT.to_owned()));
        let release = Arc::new(AtomicBool::new(false));
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (log, store, released) = (seen.clone(), stored.clone(), release.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                let (log, store, released) = (log.clone(), store.clone(), released.clone());
                let writes = writes.clone();
                std::thread::spawn(move || {
                    let Some((method, path, body)) = read_request(&stream) else {
                        return;
                    };
                    log.lock().unwrap().push(format!("{method} {path}"));
                    let (status, answer) = if !path.contains("/account_data/m.direct") {
                        (200, reply(&path).to_owned())
                    } else if method == "PUT" {
                        if writes.fetch_add(1, Ordering::SeqCst) == 0 {
                            while !released.load(Ordering::Acquire) {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                        }
                        *store.lock().unwrap() = body;
                        (200, "{}".to_owned())
                    } else {
                        (200, store.lock().unwrap().clone())
                    };
                    write_answer(&mut stream, status, &answer);
                });
            }
        });
        (address, seen, stored, release)
    }

    /// The `m.direct` requests in `seen`, by method, in the order they came.
    fn direct_methods(seen: &Seen) -> Vec<String> {
        let seen = seen.lock().unwrap();
        seen.iter()
            .filter(|request| request.contains("/account_data/m.direct"))
            .filter_map(|request| request.split(' ').next().map(str::to_owned))
            .collect()
    }

    /// Answers (joins and leaves) sent for `room` (`b` for `!b:...`).
    fn answers_for(seen: &Seen, room: &str) -> usize {
        let named = [
            format!("!{room}:"),
            format!("%21{room}%3A"),
            format!("%21{room}%3a"),
        ];
        let seen = seen.lock().unwrap();
        seen.iter()
            .filter(|request| request.ends_with("/join") || request.ends_with("/leave"))
            .filter(|request| named.iter().any(|name| request.contains(name.as_str())))
            .count()
    }

    /// A writer standing in for People's (`people_directory::PeopleGuard`,
    /// private to that module: the exact selection and its own
    /// cancellation).
    struct OtherWriter<'a> {
        selection: &'a AccountSelection,
        cancelled: AtomicBool,
    }

    impl<'a> OtherWriter<'a> {
        fn new(selection: &'a AccountSelection) -> Self {
            Self {
                selection,
                cancelled: AtomicBool::new(false),
            }
        }
    }

    impl DirectWriteGuard for OtherWriter<'_> {
        fn stopped(&self) -> bool {
            !self.selection.is_current() || self.cancelled.load(Ordering::Acquire)
        }

        fn wait_stopped(&self) -> impl Future<Output = ()> + Send {
            async move {
                while !self.stopped() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    }

    /// The account left the invitation to B by the time of this sync.
    const SYNC_B_LEFT: &str = r#"{
        "next_batch": "s2",
        "rooms": {
            "leave": {
                "!b:example.org": {
                    "timeline": {"events": [
                        {"type": "m.room.member", "state_key": "@alice:example.org",
                         "event_id": "$b-left", "sender": "@alice:example.org",
                         "origin_server_ts": 9, "content": {"membership": "leave"}}
                    ], "limited": false}
                }
            }
        }
    }"#;

    /// The bodies of the `m.direct` writes in `bodies`.
    fn direct_writes(bodies: &Seen) -> Vec<String> {
        let bodies = bodies.lock().unwrap();
        bodies
            .iter()
            .filter_map(|entry| {
                let mut parts = entry.splitn(3, ' ');
                let (method, path) = (parts.next()?, parts.next()?);
                let written = method == "PUT" && path.contains("/account_data/m.direct");
                written.then(|| parts.next().unwrap_or_default().to_owned())
            })
            .collect()
    }

    /// How many requests in `seen` touched the account's `m.direct` data.
    fn direct_requests(seen: &Seen) -> usize {
        let seen = seen.lock().unwrap();
        seen.iter()
            .filter(|request| request.contains("/account_data/m.direct"))
            .count()
    }

    /// Stored `m.direct` content: another user, an unknown field with
    /// numbers no parsed value keeps and inner spacing, and @dana's list.
    const STORED_DIRECT: &str = concat!(
        r#"{"@other:example.org": ["!x:example.org"], "#,
        r#""org.example.unknown": {"big": 123456789012345678901234567890, "decimal": 1.10}, "#,
        r#""@dana:example.org": ["!old:example.org"]}"#
    );

    /// `STORED_DIRECT` with D listed under @dana: every other value is its
    /// exact stored text.
    const LISTED_DIRECT: &str = concat!(
        r#"{"@dana:example.org":["!old:example.org","!d:example.org"],"#,
        r#""@other:example.org":["!x:example.org"],"#,
        r#""org.example.unknown":{"big": 123456789012345678901234567890, "decimal": 1.10}}"#
    );

    /// A test's own fence, open or closed for each kind of request.
    struct Gate {
        answer: AtomicBool,
        list: AtomicBool,
    }

    impl Gate {
        fn new(answer: bool, list: bool) -> Self {
            Self {
                answer: AtomicBool::new(answer),
                list: AtomicBool::new(list),
            }
        }
    }

    impl AnswerFence for Gate {
        fn may_answer(&self) -> bool {
            self.answer.load(Ordering::Acquire)
        }

        fn may_list(&self) -> bool {
            self.list.load(Ordering::Acquire)
        }
    }

    /// A listing guard for `selection` and `fence` that ends after `bound`.
    fn listing_guard<'a, F>(
        selection: &'a AccountSelection,
        fence: &'a F,
        bound: Duration,
    ) -> ListingGuard<'a, F> {
        ListingGuard {
            selection: Some(selection),
            fence,
            deadline: tokio::time::Instant::now() + bound,
        }
    }

    /// A manager whose account `id` is signed in to `homeserver` after one
    /// sync of `SYNC`, with that account's selection.
    async fn answering(
        id: &str,
        homeserver: String,
    ) -> (PathBuf, Arc<AccountManager>, AccountSelection) {
        let account = crate::AccountInfo {
            id: id.into(),
            user_id: "@alice:example.org".into(),
            device_id: "DEVICE".into(),
            homeserver,
        };
        let (directory, manager, account) = crate::history::tests::fixture(Some(account)).await;
        let client = {
            let state = manager.state.lock().await;
            state.loaded[&account.id].client.clone()
        };
        sign_in_and_sync(&client).await;
        let selection = manager.account_selection(&account.id).unwrap();
        (directory, manager, selection)
    }

    /// A client signed in to `homeserver` after one sync of `SYNC`.
    async fn synced_client(homeserver: &str) -> Client {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let client = Client::builder()
            .http_client(http)
            .homeserver_url(homeserver)
            .build()
            .await
            .unwrap();
        sign_in_and_sync(&client).await;
        client
    }

    /// Sign `client` in as @alice with a synthetic session, then sync `SYNC`
    /// once.
    async fn sign_in_and_sync(client: &Client) {
        client
            .restore_session(MatrixSession {
                meta: SessionMeta {
                    user_id: owned_user_id!("@alice:example.org"),
                    device_id: owned_device_id!("DEVICE"),
                },
                tokens: SessionTokens {
                    access_token: "token".into(),
                    refresh_token: None,
                },
            })
            .await
            .unwrap();
        client
            .sync_once(SyncSettings::default().timeout(Duration::ZERO))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn declining_leaves_only_the_invited_room_never_its_joined_predecessor() {
        let (homeserver, seen) = fake_homeserver(200);
        let client = synced_client(&homeserver).await;
        let invited = client.get_room(room_id!("!b:example.org")).unwrap();
        assert_eq!(invited.state(), RoomState::Invited);
        let predecessor = invited.predecessor_room().map(|room| room.room_id);
        assert_eq!(predecessor.as_deref(), Some(room_id!("!a:example.org")));
        let joined = client.get_room(room_id!("!a:example.org")).unwrap();
        assert_eq!(joined.state(), RoomState::Joined);

        leave_exactly(&client, invited.room_id()).await.unwrap();

        // Paths may percent-encode the room ID; compare it decoded.
        let leaves = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.ends_with("/leave"))
            .map(|request| {
                request
                    .replace("%21", "!")
                    .replace("%3A", ":")
                    .replace("%3a", ":")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            leaves,
            ["POST /_matrix/client/v3/rooms/!b:example.org/leave"]
        );
    }

    #[tokio::test]
    async fn a_refused_answer_is_not_applied_and_a_lost_one_may_have_been() {
        // A refusal (below 500) was not applied; a server error is no answer
        // at all, so the leave may have applied.
        for (status, expected) in [(403, Unanswered::NotApplied), (502, Unanswered::Unknown)] {
            let (homeserver, seen) = fake_homeserver(status);
            let client = synced_client(&homeserver).await;
            let left = leave_exactly(&client, room_id!("!b:example.org")).await;
            assert_eq!(left, Err(expected), "{status}");
            // Sent once, never retried.
            let leaves = seen
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.ends_with("/leave"))
                .count();
            assert_eq!(leaves, 1, "{status}");
        }
    }

    #[tokio::test]
    async fn an_answer_settles_only_on_the_membership_it_asked_for() {
        let (homeserver, _) = fake_homeserver(200);
        let client = synced_client(&homeserver).await;
        let (joined, invited) = (room_id!("!a:example.org"), room_id!("!b:example.org"));
        // Joined settles an accept, never a decline.
        assert!(settled(&client, joined, true));
        assert!(!settled(&client, joined, false));
        // Still an invitation, or not in the store at all: not settled.
        assert!(!settled(&client, invited, true));
        assert!(!settled(&client, invited, false));
        assert!(!settled(&client, room_id!("!missing:example.org"), true));
        // The sync wait expects that membership too: a room still invited
        // never satisfies it, whatever else the store says.
        let mut updates = client.subscribe_to_all_room_updates();
        let wait = Duration::from_millis(50);
        let joined_now = membership_synced(&client, joined, true, &mut updates, wait).await;
        let left_now = membership_synced(&client, invited, false, &mut updates, wait).await;
        assert!(joined_now && !left_now);
        // A decline is not settled by a join, though the room is no longer
        // an invitation: "anything but invited" would have said it was. A
        // room missing from the store settles nothing either.
        let left_now = membership_synced(&client, joined, false, &mut updates, wait).await;
        assert!(!left_now);
        let missing = room_id!("!missing:example.org");
        let missing_now = membership_synced(&client, missing, true, &mut updates, wait).await;
        assert!(!missing_now);
        // A room left here settles a decline, never an accept.
        let left = room_id!("!l:example.org");
        assert!(settled(&client, left, false));
        assert!(!settled(&client, left, true));
    }

    #[tokio::test]
    async fn an_answer_reports_only_what_the_homeserver_said() {
        // A refusal is an error: not applied. A server error is no answer,
        // so it may have applied. Either way the Space kind is the invite
        // state's, read when the answer was admitted.
        for (status, refused) in [(403, true), (502, false)] {
            let (homeserver, _) = fake_homeserver(status);
            let id = format!("answers-{status}");
            let (directory, manager, selection) = answering(&id, homeserver).await;
            let answer = manager
                .respond_invitation_selected(&selection, "!s:example.org", false, &Unfenced)
                .await;
            if refused {
                let Err(MatrixCoreError::Sdk(text)) = answer else {
                    panic!("a refusal is an error");
                };
                assert_eq!(text, NOT_APPLIED_DECLINE);
            } else {
                let unconfirmed = InvitationAnswer {
                    outcome: InvitationOutcome::Unconfirmed,
                    is_space: true,
                    direct: DirectMark::NotDirect,
                };
                assert_eq!(answer.unwrap(), unconfirmed);
            }
            drop(manager);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    /// A short sync wait (nothing syncs here), a roomy listing bound and the
    /// production preflight bound.
    const TEST_BOUNDS: (Duration, Duration, Duration) = (
        Duration::from_millis(50),
        Duration::from_secs(10),
        ANSWER_PREFLIGHT_BOUND,
    );

    #[tokio::test]
    async fn an_accepted_direct_invitation_is_listed_keeping_every_other_field_exactly() {
        let reads = [Some(STORED_DIRECT)];
        let (homeserver, seen, bodies) = direct_homeserver(&reads, Duration::ZERO, 200);
        let (directory, manager, selection) = answering("direct-listed", homeserver).await;
        let answer = manager
            .answer_invitation(&selection, "!d:example.org", true, &Unfenced, TEST_BOUNDS)
            .await
            .unwrap();
        // Confirmed (no sync brings it here), and listed by its one write.
        let listed = InvitationAnswer {
            outcome: InvitationOutcome::Confirmed,
            is_space: false,
            direct: DirectMark::Listed(DirectListing::Listed),
        };
        assert_eq!(answer, listed);
        // The fresh read right before the write, then exactly one write whose
        // body keeps every other user, field, number and spacing as stored:
        // no typed map rewrote it.
        assert_eq!(direct_requests(&seen), 3);
        assert_eq!(direct_writes(&bodies), [LISTED_DIRECT]);
        drop(manager);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn an_unconfirmed_or_withdrawn_answer_never_touches_the_direct_list() {
        // No answer to the join: it may have applied, but it is never
        // listed, then or later.
        let reads = [Some(STORED_DIRECT)];
        let (homeserver, seen, _) = direct_homeserver(&reads, Duration::ZERO, 502);
        let (directory, manager, selection) = answering("direct-lost", homeserver).await;
        let answer = manager
            .answer_invitation(&selection, "!d:example.org", true, &Unfenced, TEST_BOUNDS)
            .await
            .unwrap();
        assert_eq!(answer.outcome, InvitationOutcome::Unconfirmed);
        assert_eq!(answer.direct, DirectMark::NotTried);
        assert_eq!(direct_requests(&seen), 0);
        // The caller's fence closed before the hand-off: no join or leave
        // leaves at all, and the refusal says it was withdrawn.
        let closed = Gate::new(false, true);
        for (accept, withdrawn) in [(true, WITHDRAWN_ACCEPT), (false, WITHDRAWN_DECLINE)] {
            let answer = manager
                .respond_invitation_selected(&selection, "!b:example.org", accept, &closed)
                .await;
            let Err(MatrixCoreError::Sdk(text)) = answer else {
                panic!("a withdrawn answer is an error");
            };
            assert_eq!(text, withdrawn);
        }
        let answers = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.contains("!b") || request.contains("%21b"))
            .count();
        assert_eq!(answers, 0);
        // The selection ended first: nothing sent either.
        manager.publish_active_account(None);
        let answer = manager
            .respond_invitation_selected(&selection, "!b:example.org", false, &Unfenced)
            .await;
        let changed = matches!(answer, Err(MatrixCoreError::SelectedAccountChanged));
        assert!(changed);
        drop(manager);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn a_list_changed_before_the_write_or_already_listing_the_room_is_left_alone() {
        let (directory, manager, account) = crate::history::tests::fixture(None).await;
        let selection = manager.account_selection(&account.id).unwrap();
        let (dana, room) = (
            owned_user_id!("@dana:example.org"),
            room_id!("!d:example.org"),
        );
        // The fresh read differs from the first only in a number a parsed
        // value would round away: refused, nothing written.
        let changed = STORED_DIRECT.replace("890,", "891,");
        let reads = [Some(STORED_DIRECT), Some(changed.as_str())];
        let (homeserver, _, bodies) = direct_homeserver(&reads, Duration::ZERO, 200);
        let client = synced_client(&homeserver).await;
        let guard = listing_guard(&selection, &Unfenced, Duration::from_secs(10));
        let marked = mark_direct(&client, &guard, room, &dana).await;
        let refused = DirectMark::Failed(DirectWriteError::BaselineChanged);
        assert_eq!(marked, refused);
        assert!(direct_writes(&bodies).is_empty());
        // Odd stored content is refused, not rewritten.
        let odd = [Some(r#"{"@dana:example.org": "!old:example.org"}"#)];
        let (homeserver, _, bodies) = direct_homeserver(&odd, Duration::ZERO, 200);
        let client = synced_client(&homeserver).await;
        let marked = mark_direct(&client, &guard, room, &dana).await;
        assert_eq!(marked, DirectMark::Failed(DirectWriteError::Unreadable));
        assert!(direct_writes(&bodies).is_empty());
        // Listed already: one read, nothing written.
        let listed = [Some(r#"{"@dana:example.org": ["!d:example.org"]}"#)];
        let (homeserver, seen, bodies) = direct_homeserver(&listed, Duration::ZERO, 200);
        let client = synced_client(&homeserver).await;
        let marked = mark_direct(&client, &guard, room, &dana).await;
        assert_eq!(marked, DirectMark::Listed(DirectListing::AlreadyListed));
        assert_eq!(direct_requests(&seen), 1);
        assert!(direct_writes(&bodies).is_empty());
        drop(manager);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn a_listing_sends_nothing_once_its_fence_bound_or_selection_ended() {
        let (directory, manager, account) = crate::history::tests::fixture(None).await;
        let selection = manager.account_selection(&account.id).unwrap();
        let (dana, room) = (
            owned_user_id!("@dana:example.org"),
            room_id!("!d:example.org"),
        );
        let reads = [Some(STORED_DIRECT)];
        let (homeserver, seen, _) = direct_homeserver(&reads, Duration::ZERO, 200);
        let client = synced_client(&homeserver).await;
        let not_sent = DirectMark::Failed(DirectWriteError::NotSent);
        // The caller's fence closed for listings (a security step, say).
        let closed = Gate::new(true, false);
        let guard = listing_guard(&selection, &closed, Duration::from_secs(10));
        assert_eq!(mark_direct(&client, &guard, room, &dana).await, not_sent);
        // The listing's own bound already passed.
        let late = listing_guard(&selection, &Unfenced, Duration::ZERO);
        assert_eq!(mark_direct(&client, &late, room, &dana).await, not_sent);
        // Another selection now.
        manager.publish_active_account(None);
        let guard = listing_guard(&selection, &Unfenced, Duration::from_secs(10));
        assert_eq!(mark_direct(&client, &guard, room, &dana).await, not_sent);
        assert_eq!(direct_requests(&seen), 0);
        drop(manager);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn a_listing_whose_bound_ends_during_its_write_is_unknown_and_never_resent() {
        let (directory, manager, account) = crate::history::tests::fixture(None).await;
        let selection = manager.account_selection(&account.id).unwrap();
        let (dana, room) = (
            owned_user_id!("@dana:example.org"),
            room_id!("!d:example.org"),
        );
        // The write is held well past the listing's bound.
        let reads = [Some(STORED_DIRECT)];
        let (homeserver, _, bodies) = direct_homeserver(&reads, Duration::from_secs(3), 200);
        let client = synced_client(&homeserver).await;
        let guard = listing_guard(&selection, &Unfenced, Duration::from_millis(800));
        let marked = mark_direct(&client, &guard, room, &dana).await;
        assert_eq!(marked, DirectMark::Failed(DirectWriteError::Unknown));
        // It was handed over once; nothing sends it again.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(direct_writes(&bodies), [LISTED_DIRECT]);
        drop(manager);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn a_join_answer_naming_another_room_is_unconfirmed_and_never_listed() {
        // A valid room ID, but not the one asked for: no confirmation.
        assert_eq!(
            joined_exactly(room_id!("!d:example.org"), room_id!("!other:example.org")),
            Err(Unanswered::Unknown)
        );
        assert_eq!(
            joined_exactly(room_id!("!d:example.org"), room_id!("!d:example.org")),
            Ok(())
        );
        let (homeserver, seen, _) = loopback(|_, path| {
            if path.ends_with("/join") {
                (200, r#"{"room_id": "!other:example.org"}"#.to_owned())
            } else if path.contains("/account_data/m.direct") {
                (200, STORED_DIRECT.to_owned())
            } else {
                (200, reply(path).to_owned())
            }
        });
        let (directory, manager, selection) = answering("join-elsewhere", homeserver).await;
        let answer = manager
            .answer_invitation(&selection, "!d:example.org", true, &Unfenced, TEST_BOUNDS)
            .await
            .unwrap();
        // Handed over and answered for another room: it may have done
        // something, it is not a refusal, and nothing follows from it.
        let unconfirmed = InvitationAnswer {
            outcome: InvitationOutcome::Unconfirmed,
            is_space: false,
            direct: DirectMark::NotTried,
        };
        assert_eq!(answer, unconfirmed);
        assert_eq!(direct_requests(&seen), 0);
        // One join, for the requested room only.
        assert_eq!(answers_for(&seen, "d"), 1);
        assert_eq!(answers_for(&seen, "other"), 0);
        drop(manager);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn an_invitation_that_sync_ended_during_the_preflight_gets_no_answer() {
        let left = Arc::new(AtomicBool::new(false));
        let serving = left.clone();
        let (homeserver, seen, _) = loopback(move |_, path| {
            if path.starts_with("/_matrix/client/v3/sync") && serving.load(Ordering::Acquire) {
                (200, SYNC_B_LEFT.to_owned())
            } else {
                (200, reply(path).to_owned())
            }
        });
        let (directory, manager, selection) = answering("preflight-left", homeserver).await;
        let client = {
            let state = manager.state.lock().await;
            state.loaded[selection.account_id()].client.clone()
        };
        // The answer is held right after its preflight (its manager and
        // invite-state reads), while sync brings the room as left.
        let entered = Arc::new(tokio::sync::Notify::new());
        let proceed = Arc::new(tokio::sync::Notify::new());
        let account = selection.account_id().to_owned();
        let hold = (account, entered.clone(), proceed.clone());
        HELD_PREFLIGHTS.lock().unwrap().push(hold);
        let answer =
            manager.answer_invitation(&selection, "!b:example.org", true, &Unfenced, TEST_BOUNDS);
        let leave_meanwhile = async {
            entered.notified().await;
            left.store(true, Ordering::Release);
            let settings = SyncSettings::default().timeout(Duration::ZERO);
            client.sync_once(settings).await.unwrap();
            let room = client.get_room(room_id!("!b:example.org")).unwrap();
            assert_eq!(room.state(), RoomState::Left);
            proceed.notify_one();
        };
        let (answer, ()) = tokio::join!(answer, leave_meanwhile);
        // Nothing was sent, and the result says the invitation is gone.
        assert!(matches!(answer, Err(MatrixCoreError::RoomNotFound(_))));
        assert_eq!(answers_for(&seen, "b"), 0);
        drop(manager);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn a_held_manager_wait_ends_with_the_fence_or_its_bound_and_sends_nothing() {
        let (homeserver, seen) = fake_homeserver(200);
        let (directory, manager, selection) = answering("manager-held", homeserver).await;
        // The manager's state is held through the answer's admission, and
        // the view it was chosen in closes meanwhile.
        let gate = Gate::new(true, true);
        let held = manager.state.lock().await;
        let answer =
            manager.answer_invitation(&selection, "!b:example.org", true, &gate, TEST_BOUNDS);
        let closing = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            gate.answer.store(false, Ordering::Release);
        };
        let (answer, ()) = tokio::join!(answer, closing);
        let Err(MatrixCoreError::Sdk(text)) = answer else {
            panic!("withdrawn before it was sent");
        };
        assert_eq!(text, WITHDRAWN_ACCEPT);
        // Released only after the answer ended: nothing goes later either.
        drop(held);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(answers_for(&seen, "b"), 0);
        // With the fence still open, the wait's own bound ends it: nothing
        // sent, and not an unknown outcome.
        let open = Gate::new(true, true);
        let held = manager.state.lock().await;
        let bounds = (TEST_BOUNDS.0, TEST_BOUNDS.1, Duration::from_millis(300));
        let answer = manager
            .answer_invitation(&selection, "!b:example.org", false, &open, bounds)
            .await;
        assert!(matches!(answer, Err(MatrixCoreError::AccountAccessBusy)));
        drop(held);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(answers_for(&seen, "b"), 0);
        drop(manager);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn every_in_process_direct_listing_waits_its_turn_so_none_overwrites_another() {
        let (directory, manager, account) = crate::history::tests::fixture(None).await;
        let selection = manager.account_selection(&account.id).unwrap();
        let (homeserver, seen, stored, release) = direct_store_homeserver();
        let client = synced_client(&homeserver).await;
        let own = client.user_id().unwrap().to_owned();
        let dana = owned_user_id!("@dana:example.org");
        let erin = owned_user_id!("@erin:example.org");
        // An invitation's listing reads its baselines and its write is held;
        // People's writer starts meanwhile, and a third writer stops while
        // waiting.
        let invitation = listing_guard(&selection, &Unfenced, Duration::from_secs(10));
        let people = OtherWriter::new(&selection);
        let stopped = OtherWriter::new(&selection);
        let first = list_direct_room(
            &client,
            &own,
            &dana,
            room_id!("!d:example.org"),
            &invitation,
        );
        let rest = async {
            while !direct_methods(&seen).contains(&"PUT".to_owned()) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let held = direct_methods(&seen).len();
            let (e, f) = (room_id!("!e:example.org"), room_id!("!f:example.org"));
            let second = list_direct_room(&client, &own, &erin, e, &people);
            let third = list_direct_room(&client, &own, &erin, f, &stopped);
            let checks = async {
                // Neither reads while the first holds the turn.
                tokio::time::sleep(Duration::from_millis(300)).await;
                assert_eq!(direct_methods(&seen).len(), held);
                stopped.cancelled.store(true, Ordering::Release);
                tokio::time::sleep(Duration::from_millis(100)).await;
                release.store(true, Ordering::Release);
            };
            let (second, third, ()) = tokio::join!(second, third, checks);
            (second, third)
        };
        let (first, (second, third)) = tokio::join!(first, rest);
        assert_eq!(first, Ok(DirectListing::Listed));
        assert_eq!(second, Ok(DirectListing::Listed));
        // The stopped waiter sent nothing.
        assert_eq!(third, Err(DirectWriteError::NotSent));
        // The second read the first's write as its baseline: both survive.
        let stored = stored.lock().unwrap().clone();
        assert!(stored.contains(r#""!d:example.org""#), "{stored}");
        assert!(stored.contains(r#""!e:example.org""#), "{stored}");
        assert!(!stored.contains("!f:example.org"), "{stored}");
        let order = ["GET", "GET", "PUT", "GET", "GET", "PUT"];
        assert_eq!(direct_methods(&seen), order);
        drop(manager);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn summaries_show_only_supplied_bounded_text() {
        assert_eq!(supplied(None), None);
        assert_eq!(supplied(Some("   ")), None);
        assert_eq!(supplied(Some("  Team  ")).as_deref(), Some("Team"));
        let long = "a".repeat(MAX_INVITATION_TEXT_CHARS + 10);
        assert_eq!(
            supplied(Some(&long)).map(|text| text.chars().count()),
            Some(MAX_INVITATION_TEXT_CHARS)
        );
    }

    #[test]
    fn refused_answers_report_fixed_wording() {
        // A lost answer may still have joined or left: never "not accepted".
        for what in [
            NOT_APPLIED_ACCEPT,
            NOT_APPLIED_DECLINE,
            UNCONFIRMED_ACCEPT,
            UNCONFIRMED_DECLINE,
            WITHDRAWN_ACCEPT,
            WITHDRAWN_DECLINE,
        ] {
            let shown =
                refused::<String>(what)("M_FORBIDDEN https://hs.example/?t=1".into()).to_string();
            assert!(shown.contains(what) && !shown.contains("M_FORBIDDEN"));
        }
    }
}
