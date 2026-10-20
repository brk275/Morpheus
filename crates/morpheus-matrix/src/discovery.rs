//! Public room discovery and Matrix links. Directory results are untrusted
//! homeserver metadata, so the UI receives plain display fields rather than
//! SDK clients, event content, or credentials.
//!
//! Joining (`join_room_for`) is one bounded request for the exact selected
//! account:
//! - the typed join with at most MAX_JOIN_VIA validated routes; the server
//!   resolves an alias inside that request;
//! - no HTTP retries and no automatic token refresh and replay, a timeout
//!   and a small reply;
//! - no lock held across the send;
//! - revocation wins over a send that has not started (root's selection
//!   guard).
//! Before the hand-off nothing is sent; after it, a missing reply is an
//! unknown outcome, never sent again. A definite 4xx is a fixed category,
//! and no homeserver text is kept. Nothing local changes: the room appears
//! once sync shows it joined (`await_joined_for`).
//!
//! The requester's withdrawal (`JoinCancellation`, a Close or Escape) and
//! the hand-off are one atomic decision: a withdrawal that wins sends
//! nothing; one that loses leaves the request to its own bounds and
//! withdraws only the wait for sync. Every wait before the hand-off (the
//! account's client, under the manager's lock) is bounded and withdrawable.
//! A typed room ID is joined exactly: a reply naming another room is never
//! taken as that room.
//! The join configuration limits the final POST; SDK metadata/cache setup
//! before that POST has its own behavior and is not globally capped here.
//!
//! `disable_auth_refresh()` is root's SDK change d4c5d08, not in this base
//! (c7d0e12): this module compiles only once combined with it.

use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use std::time::Duration;

use matrix_sdk::{
    Client, HttpError, RoomState,
    config::RequestConfig,
    ruma::{
        MatrixToUri, MatrixUri, OwnedRoomId, OwnedRoomOrAliasId, OwnedServerName, RoomAliasId,
        RoomId, RoomOrAliasId, ServerName, UserId,
        api::{
            client::{directory::get_public_rooms_filtered, membership::join_room_by_id_or_alias},
            error::ErrorKind,
        },
        directory::{Filter, PublicRoomsChunk},
        matrix_uri::MatrixId,
        room::RoomType,
    },
};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::{AccountManager, AccountSelection, MatrixCoreError, Result, selected_loaded_for};

/// Routing servers one join names at most.
pub const MAX_JOIN_VIA: usize = 10;
const JOIN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_JOIN_REPLY_BYTES: u64 = 64 * 1024;
/// How long a confirmed join waits for sync to show the room joined.
const JOIN_SYNC_WAIT: Duration = Duration::from_secs(15);
const JOIN_SYNC_POLL: Duration = Duration::from_millis(250);
/// How long a join waits for the account's client (the manager's lock).
/// Two such waits, the request and the sync wait stay under a minute.
const JOIN_CLIENT_WAIT: Duration = Duration::from_secs(5);

/// Why a join did not complete, as a fixed sentence: the homeserver's own
/// text is never kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum JoinError {
    #[error("The account changed before the join was sent. Nothing was sent.")]
    AccountChanged,
    #[error("Finish the current security step first. Nothing was sent.")]
    Blocked,
    #[error("That is not a room or room address Morpheus can join.")]
    InvalidTarget,
    #[error("That link names more servers than Morpheus joins through.")]
    TooManyRoutes,
    #[error("The join was cancelled before it was sent. Nothing was sent.")]
    Cancelled,
    #[error("A join of this room is still waiting for its reply. Nothing more was sent.")]
    Pending,
    #[error(
        "Other joins still need checking. Further joins are paused in this session. Nothing was sent."
    )]
    UnresolvedLimit,
    /// An earlier join of the same room for this account had no definite
    /// reply. Further joins of this key stay paused; a lost UI error must
    /// never silently authorize resending it.
    #[error(
        "An earlier join of this room may or may not have happened. Further joins of it are paused in this session. Check your room list. Nothing was sent."
    )]
    EarlierUnknown,
    #[error("You are not allowed to join that room. Nothing was joined.")]
    NotAllowed,
    #[error("That room or address was not found, or no server could reach it. Nothing was joined.")]
    NotFound,
    #[error("Your homeserver is limiting requests. Nothing was joined; try again shortly.")]
    RateLimited,
    #[error("Your homeserver refused the join. Nothing was joined.")]
    Rejected,
    /// The request was handed off and no definite reply came.
    #[error(
        "No reply arrived, so you may or may not have joined. Check your room list before joining again."
    )]
    OutcomeUnknown,
    /// A typed room ID was joined and the reply named another room: nothing
    /// is opened, and whether either room was joined is not known.
    #[error(
        "The homeserver answered for a different room, so nothing was opened. Check your room list before joining again."
    )]
    UnexpectedRoom,
    #[error("This account cannot join rooms right now. Nothing was sent.")]
    Unavailable,
}

impl JoinError {
    /// Whether the join may have happened.
    pub fn may_have_joined(self) -> bool {
        matches!(self, Self::OutcomeUnknown | Self::UnexpectedRoom)
    }
}

const JOIN_SENT: u8 = 1;
const JOIN_WITHDRAWN: u8 = 2;

/// One join's withdrawal, shared by its requester (a Close or Escape) and
/// the join. Withdrawing and handing the request off are one atomic choice:
/// whichever comes first wins, so a withdrawal either sent nothing or came
/// after the request left. After the hand-off the request keeps its own
/// bounds and is never dropped by a withdrawal; only the wait for sync (and
/// opening the room) is withdrawn.
#[derive(Clone, Default)]
pub struct JoinCancellation(Arc<JoinCancellationState>);

#[derive(Default)]
struct JoinCancellationState {
    state: AtomicU8,
    notify: Notify,
}

impl JoinCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Withdraw the join. True when it was withdrawn before its request was
    /// handed off (nothing was sent), false once it had been.
    pub fn cancel(&self) -> bool {
        let before = self.0.state.fetch_or(JOIN_WITHDRAWN, Ordering::AcqRel);
        self.0.notify.notify_waiters();
        before & JOIN_SENT == 0
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.state.load(Ordering::Acquire) & JOIN_WITHDRAWN != 0
    }

    /// Whether the request was handed off, whatever happened since.
    pub fn sent(&self) -> bool {
        self.0.state.load(Ordering::Acquire) & JOIN_SENT != 0
    }

    /// Take the hand-off: false (nothing may be sent) once withdrawn.
    fn begin_send(&self) -> bool {
        self.0
            .state
            .compare_exchange(0, JOIN_SENT, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Wait until withdrawn. Registration precedes the state check, so a
    /// withdrawal cannot be missed while going to sleep.
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

/// A request against the selected account's homeserver. `server` is a Matrix
/// server name for the homeserver to query through federation, not a URL to
/// contact directly. The homeserver may decline or lack federation support.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicRoomsQuery {
    pub search_term: Option<String>,
    pub server: Option<String>,
    pub since: Option<String>,
    pub limit: u32,
}

impl Default for PublicRoomsQuery {
    fn default() -> Self {
        Self {
            search_term: None,
            server: None,
            since: None,
            limit: 25,
        }
    }
}

/// Display-only directory entry. Names and topics come from other users and
/// must remain plain text in the native UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicRoomInfo {
    pub room_id: String,
    pub canonical_alias: Option<String>,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub avatar_mxc: Option<String>,
    pub joined_members: u64,
    pub world_readable: bool,
    pub guest_can_join: bool,
    pub join_rule: String,
    pub is_space: bool,
}

impl From<PublicRoomsChunk> for PublicRoomInfo {
    fn from(room: PublicRoomsChunk) -> Self {
        Self {
            room_id: room.room_id.to_string(),
            canonical_alias: room.canonical_alias.map(|alias| alias.to_string()),
            name: room.name,
            topic: room.topic,
            avatar_mxc: room.avatar_url.map(|url| url.to_string()),
            joined_members: room.num_joined_members.into(),
            world_readable: room.world_readable,
            guest_can_join: room.guest_can_join,
            join_rule: room.join_rule.as_str().to_owned(),
            is_space: matches!(room.room_type, Some(RoomType::Space)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicRoomsPage {
    pub rooms: Vec<PublicRoomInfo>,
    pub next_batch: Option<String>,
    pub prev_batch: Option<String>,
    pub total_room_count_estimate: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRoomAlias {
    pub room_id: String,
    pub via_servers: Vec<String>,
}

/// A parsed raw Matrix identifier, `matrix.to` link, or `matrix:` URI. Event
/// links retain their room so callers can navigate there after joining.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MatrixLinkTarget {
    Room {
        room_or_alias_id: String,
        event_id: Option<String>,
        via_servers: Vec<String>,
    },
    User {
        user_id: String,
    },
}

/// Parse Matrix navigation input without fetching a URL or trusting its host.
/// Arbitrary web URLs and event IDs without a room are rejected.
pub fn parse_matrix_link_target(input: &str) -> Result<MatrixLinkTarget> {
    let input = input.trim();
    if input.is_empty() || input.len() > 4096 {
        return Err(MatrixCoreError::InvalidMatrixLink(
            "link is empty or too long".into(),
        ));
    }
    if input.starts_with("https://matrix.to/") {
        let uri = MatrixToUri::parse(input)
            .map_err(|e| MatrixCoreError::InvalidMatrixLink(e.to_string()))?;
        return target_from_id(uri.id(), uri.via());
    }
    if input.starts_with("matrix:") {
        let uri = MatrixUri::parse(input)
            .map_err(|e| MatrixCoreError::InvalidMatrixLink(e.to_string()))?;
        return target_from_id(uri.id(), uri.via());
    }
    if let Ok(room) = RoomOrAliasId::parse(input) {
        return Ok(MatrixLinkTarget::Room {
            room_or_alias_id: room.to_string(),
            event_id: None,
            via_servers: Vec::new(),
        });
    }
    if let Ok(user) = UserId::parse(input) {
        return Ok(MatrixLinkTarget::User {
            user_id: user.to_string(),
        });
    }
    Err(MatrixCoreError::InvalidMatrixLink(
        "unrecognized Matrix identifier or URI".into(),
    ))
}

fn target_from_id(
    id: &MatrixId,
    via: &[matrix_sdk::ruma::OwnedServerName],
) -> Result<MatrixLinkTarget> {
    let via_servers = via.iter().map(ToString::to_string).collect();
    Ok(match id {
        MatrixId::Room(id) => MatrixLinkTarget::Room {
            room_or_alias_id: id.to_string(),
            event_id: None,
            via_servers,
        },
        MatrixId::RoomAlias(id) => MatrixLinkTarget::Room {
            room_or_alias_id: id.to_string(),
            event_id: None,
            via_servers,
        },
        MatrixId::Event(room, event) => MatrixLinkTarget::Room {
            room_or_alias_id: room.to_string(),
            event_id: Some(event.to_string()),
            via_servers,
        },
        MatrixId::User(id) => MatrixLinkTarget::User {
            user_id: id.to_string(),
        },
        _ => {
            return Err(MatrixCoreError::InvalidMatrixLink(
                "unsupported Matrix link target".into(),
            ));
        }
    })
}

impl AccountManager {
    /// Fetch one page from a public room directory for the account currently
    /// on screen. A response received after switching accounts is discarded.
    pub async fn public_rooms_for(
        &self,
        expected_account_id: &str,
        query: PublicRoomsQuery,
    ) -> Result<PublicRoomsPage> {
        let client = {
            let state = self.state.lock().await;
            selected_loaded_for(&state, expected_account_id)?
                .client
                .clone()
        };
        let mut request = get_public_rooms_filtered::v3::Request::new();
        let search = query
            .search_term
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if search.is_some_and(|s| s.len() > 256 || s.chars().any(char::is_control)) {
            return Err(MatrixCoreError::InvalidDirectoryQuery(
                "search must be at most 256 bytes of plain text".into(),
            ));
        }
        if query
            .since
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 4096)
        {
            return Err(MatrixCoreError::InvalidDirectoryQuery(
                "pagination token is empty or too long".into(),
            ));
        }
        let mut filter = Filter::new();
        filter.generic_search_term = search.map(str::to_owned);
        request.filter = filter;
        request.server = query
            .server
            .as_deref()
            .map(|server| {
                ServerName::parse(server)
                    .map_err(|e| MatrixCoreError::InvalidDirectoryQuery(e.to_string()))
            })
            .transpose()?;
        request.since = query.since;
        request.limit = Some(query.limit.clamp(1, 100).into());
        let response = client
            .public_rooms_filtered(request)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        {
            let state = self.state.lock().await;
            selected_loaded_for(&state, expected_account_id)?;
        }
        Ok(PublicRoomsPage {
            rooms: response.chunk.into_iter().map(Into::into).collect(),
            next_batch: response.next_batch,
            prev_batch: response.prev_batch,
            total_room_count_estimate: response.total_room_count_estimate.map(Into::into),
        })
    }

    /// Resolve an alias through the selected homeserver. Remote aliases need
    /// working federation on that server.
    pub async fn resolve_room_alias_for(
        &self,
        expected_account_id: &str,
        alias: &str,
    ) -> Result<ResolvedRoomAlias> {
        let alias =
            RoomAliasId::parse(alias).map_err(|e| MatrixCoreError::InvalidRoomId(e.to_string()))?;
        let client = {
            let state = self.state.lock().await;
            selected_loaded_for(&state, expected_account_id)?
                .client
                .clone()
        };
        let response = client
            .resolve_room_alias(&alias)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        {
            let state = self.state.lock().await;
            selected_loaded_for(&state, expected_account_id)?;
        }
        Ok(ResolvedRoomAlias {
            room_id: response.room_id.to_string(),
            via_servers: response
                .servers
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
        })
    }

    /// Join a parsed room ID, alias, or room or event link for the exact
    /// `selection`, using its `via` servers, in one bounded request (see the
    /// module notes). `admit` is the caller's own check (the app's security
    /// gate), asked at the hand-off with the selection and `cancellation`,
    /// with no await before the send. Returns the joined room's ID: for a
    /// typed room ID exactly that room. Nothing local changes here.
    pub async fn join_room_for(
        &self,
        selection: &AccountSelection,
        target: &MatrixLinkTarget,
        cancellation: &JoinCancellation,
        admit: impl Fn() -> bool,
    ) -> std::result::Result<String, JoinError> {
        let (id, via) = join_route(target)?;
        let client = self.join_client(selection, cancellation).await?;
        let mut request = join_room_by_id_or_alias::v3::Request::new(id.clone());
        request.via = via;
        let sending = async {
            client
                .send(request)
                .with_request_config(join_config())
                .await
        };
        let owned = self.owns_selection(selection);
        let response = hand_off(selection, owned, cancellation, admit(), sending)
            .await?
            .map_err(|error| join_failure(&error))?;
        Ok(joined_room(&id, &response.room_id)?.to_string())
    }

    /// The account's client for a join: the manager's lock is waited for at
    /// most JOIN_CLIENT_WAIT, under the selection and `cancellation`.
    async fn join_client(
        &self,
        selection: &AccountSelection,
        cancellation: &JoinCancellation,
    ) -> std::result::Result<Client, JoinError> {
        if cancellation.is_cancelled() {
            return Err(JoinError::Cancelled);
        }
        let acquiring =
            tokio::time::timeout(JOIN_CLIENT_WAIT, self.hierarchy_client_for(selection));
        let acquired = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(JoinError::Cancelled),
            acquired = acquiring => acquired,
        };
        match acquired {
            Ok(Ok(client)) => Ok(client),
            Ok(Err(MatrixCoreError::SelectedAccountChanged)) => Err(JoinError::AccountChanged),
            // A stalled manager or an ineligible account: nothing was sent.
            Ok(Err(_)) | Err(_) => Err(JoinError::Unavailable),
        }
    }

    /// Wait, at most JOIN_SYNC_WAIT, for sync to show `room_id` joined for
    /// the exact `selection`: `Some(is_space)` then, `None` if it did not in
    /// time, the selection departed or the join was withdrawn. Sends
    /// nothing and changes nothing.
    pub async fn await_joined_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        cancellation: &JoinCancellation,
    ) -> Option<bool> {
        let room_id = RoomId::parse(room_id).ok()?;
        let client = self.join_client(selection, cancellation).await.ok()?;
        let joined = async {
            loop {
                if let Some(room) = client
                    .get_room(&room_id)
                    .filter(|room| room.state() == RoomState::Joined)
                {
                    return room.is_space();
                }
                tokio::time::sleep(JOIN_SYNC_POLL).await;
            }
        };
        let waited = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return None,
            waited = selection.guard(tokio::time::timeout(JOIN_SYNC_WAIT, joined)) => waited?,
        };
        let is_space = waited.ok()?;
        (self.owns_selection(selection) && !cancellation.is_cancelled()).then_some(is_space)
    }
}

/// The room a join's reply names: for a typed room ID exactly that room
/// (a reply naming any other is not trusted, and may have joined
/// something), for an alias the room the homeserver resolved it to.
pub(crate) fn joined_room(
    asked: &RoomOrAliasId,
    answered: &RoomId,
) -> std::result::Result<OwnedRoomId, JoinError> {
    if asked.is_room_id() && asked.as_str() != answered.as_str() {
        return Err(JoinError::UnexpectedRoom);
    }
    Ok(answered.to_owned())
}

/// The room and the validated routes of a join: a room link only, at most
/// MAX_JOIN_VIA routes, each a valid server name, without repeats.
pub(crate) fn join_route(
    target: &MatrixLinkTarget,
) -> std::result::Result<(OwnedRoomOrAliasId, Vec<OwnedServerName>), JoinError> {
    let MatrixLinkTarget::Room {
        room_or_alias_id,
        via_servers,
        ..
    } = target
    else {
        return Err(JoinError::InvalidTarget);
    };
    let id = RoomOrAliasId::parse(room_or_alias_id).map_err(|_| JoinError::InvalidTarget)?;
    if via_servers.len() > MAX_JOIN_VIA {
        return Err(JoinError::TooManyRoutes);
    }
    let mut via = Vec::with_capacity(via_servers.len());
    for server in via_servers {
        let server = ServerName::parse(server).map_err(|_| JoinError::InvalidTarget)?;
        if !via.contains(&server) {
            via.push(server);
        }
    }
    Ok((id, via))
}

/// Hand one request off for `selection`, raced against its revocation with
/// root's selection guard (cancellation first, d56ba8b). Nothing is sent
/// unless the requester has not withdrawn, the selection is still owned
/// and current and `admitted` holds, all checked with no await before the
/// send; the hand-off itself is taken atomically against a withdrawal.
/// When the guard drops the request, it was never sent if it had not been
/// handed off (AccountChanged), and may have been otherwise
/// (OutcomeUnknown). It is never sent again.
pub(crate) async fn hand_off<F: Future>(
    selection: &AccountSelection,
    owned: bool,
    cancellation: &JoinCancellation,
    admitted: bool,
    send: F,
) -> std::result::Result<F::Output, JoinError> {
    if cancellation.is_cancelled() {
        return Err(JoinError::Cancelled);
    }
    if !owned || !selection.is_current() {
        return Err(JoinError::AccountChanged);
    }
    if !admitted {
        return Err(JoinError::Blocked);
    }
    let sending = async {
        if !cancellation.begin_send() {
            return None;
        }
        Some(send.await)
    };
    match selection.guard(sending).await {
        Some(Some(output)) => Ok(output),
        Some(None) => Err(JoinError::Cancelled),
        None if cancellation.sent() => Err(JoinError::OutcomeUnknown),
        None => Err(JoinError::AccountChanged),
    }
}

/// One attempt on the wire: no HTTP retries, no automatic token refresh and
/// replay (root's d4c5d08), a timeout and a small reply.
pub(crate) fn join_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .timeout(JOIN_TIMEOUT)
        .max_response_bytes(MAX_JOIN_REPLY_BYTES)
        .disable_auth_refresh()
}

/// A definite 4xx joined nothing; anything else may have.
pub(crate) fn join_failure(error: &HttpError) -> JoinError {
    let Some(api) = error.as_client_api_error() else {
        return JoinError::OutcomeUnknown;
    };
    if !api.status_code.is_client_error() {
        return JoinError::OutcomeUnknown;
    }
    match api.error_kind() {
        // Without the replay, an expired token returns its 401 as is.
        Some(ErrorKind::UnknownToken { .. }) => JoinError::OutcomeUnknown,
        Some(ErrorKind::Forbidden) => JoinError::NotAllowed,
        Some(ErrorKind::NotFound) => JoinError::NotFound,
        Some(ErrorKind::LimitExceeded(_)) => JoinError::RateLimited,
        _ if api.status_code.as_u16() == 429 => JoinError::RateLimited,
        _ => JoinError::Rejected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_error(status: u16, errcode: &str) -> HttpError {
        use matrix_sdk::ruma::api::{
            client::uiaa::UiaaResponse,
            error::{Error, ErrorBody, FromHttpResponseError, StandardErrorBody},
        };
        let body: StandardErrorBody = serde_json::from_value(serde_json::json!({
            "errcode": errcode, "error": "private reflected response"
        }))
        .unwrap();
        HttpError::Api(Box::new(FromHttpResponseError::Server(
            UiaaResponse::MatrixError(Error::new(
                reqwest::StatusCode::from_u16(status).unwrap(),
                ErrorBody::Standard(body),
            )),
        )))
    }

    fn room(via: &[&str]) -> MatrixLinkTarget {
        MatrixLinkTarget::Room {
            room_or_alias_id: "#lobby:example.org".into(),
            event_id: None,
            via_servers: via.iter().map(|server| (*server).to_owned()).collect(),
        }
    }

    #[test]
    fn a_join_names_one_room_and_at_most_ten_valid_routes() {
        let (id, via) = join_route(&room(&["one.org", "two.org", "one.org"])).unwrap();
        assert_eq!(id.as_str(), "#lobby:example.org");
        let via = via.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(via, ["one.org", "two.org"], "repeats are sent once");
        let ten = (0..MAX_JOIN_VIA)
            .map(|n| format!("s{n}.org"))
            .collect::<Vec<_>>();
        let ten = ten.iter().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(join_route(&room(&ten)).unwrap().1.len(), MAX_JOIN_VIA);
        let mut eleven = ten.clone();
        eleven.push("s10.org");
        assert_eq!(join_route(&room(&eleven)), Err(JoinError::TooManyRoutes));
        assert_eq!(
            join_route(&room(&["not a server"])),
            Err(JoinError::InvalidTarget)
        );
        let user = MatrixLinkTarget::User {
            user_id: "@alice:example.org".into(),
        };
        assert_eq!(join_route(&user), Err(JoinError::InvalidTarget));
        let bad = MatrixLinkTarget::Room {
            room_or_alias_id: "lobby".into(),
            event_id: None,
            via_servers: Vec::new(),
        };
        assert_eq!(join_route(&bad), Err(JoinError::InvalidTarget));
    }

    #[test]
    fn the_join_is_one_capped_attempt_and_failures_keep_no_server_text() {
        let config = format!("{:?}", join_config());
        assert!(config.contains("retry_limit: 0"), "{config}");
        assert!(
            config.contains(&format!("max_response_bytes: {MAX_JOIN_REPLY_BYTES}")),
            "{config}"
        );
        for (status, errcode, expected) in [
            (403, "M_FORBIDDEN", JoinError::NotAllowed),
            (404, "M_NOT_FOUND", JoinError::NotFound),
            (429, "M_LIMIT_EXCEEDED", JoinError::RateLimited),
            (429, "M_UNKNOWN", JoinError::RateLimited),
            (400, "M_BAD_JSON", JoinError::Rejected),
        ] {
            let failure = join_failure(&api_error(status, errcode));
            assert_eq!(failure, expected, "{errcode}");
            assert!(!failure.may_have_joined());
            assert!(!failure.to_string().contains("private"));
            assert!(
                failure.to_string().contains("Nothing was joined"),
                "{failure}"
            );
        }
        // Without the replay an expired token is unconfirmed; so is a server
        // error or a reply too large to read.
        for error in [
            api_error(401, "M_UNKNOWN_TOKEN"),
            api_error(502, "M_UNKNOWN"),
            HttpError::ResponseTooLarge {
                limit: MAX_JOIN_REPLY_BYTES,
            },
        ] {
            let failure = join_failure(&error);
            assert_eq!(failure, JoinError::OutcomeUnknown);
            assert!(failure.may_have_joined());
            assert!(failure.to_string().contains("may or may not have joined"));
        }
        // Converted for the app, the text stays the fixed sentence.
        let core: MatrixCoreError = JoinError::NotAllowed.into();
        assert_eq!(core.to_string(), JoinError::NotAllowed.to_string());
    }

    #[test]
    fn nothing_is_sent_unless_the_exact_selection_and_the_caller_admit_it() {
        use crate::account_selection::Selections;
        use std::sync::{Arc, atomic::AtomicUsize};
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let selections = Selections::default();
            selections.replace(Some("a"));
            let selection = selections.for_account("a").unwrap();
            let sends = Arc::new(AtomicUsize::new(0));
            let send = || {
                let sends = sends.clone();
                async move {
                    sends.fetch_add(1, Ordering::SeqCst);
                    "!joined:example.org"
                }
            };
            // A refusing gate, or a selection this manager no longer owns.
            assert_eq!(
                hand_off(&selection, true, &JoinCancellation::new(), false, send()).await,
                Err(JoinError::Blocked)
            );
            assert_eq!(
                hand_off(&selection, false, &JoinCancellation::new(), true, send()).await,
                Err(JoinError::AccountChanged)
            );
            assert_eq!(sends.load(Ordering::SeqCst), 0);
            // Admitted: one request, its answer kept.
            assert_eq!(
                hand_off(&selection, true, &JoinCancellation::new(), true, send()).await,
                Ok("!joined:example.org")
            );
            assert_eq!(sends.load(Ordering::SeqCst), 1);
            // The account changed before the hand-off: nothing is sent.
            selections.replace(Some("b"));
            assert_eq!(
                hand_off(&selection, true, &JoinCancellation::new(), true, send()).await,
                Err(JoinError::AccountChanged)
            );
            assert_eq!(sends.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn a_held_join_cancelled_after_its_hand_off_is_unknown_and_never_resent() {
        use crate::account_selection::Selections;
        use std::sync::{Arc, atomic::AtomicUsize};
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let selections = Selections::default();
            selections.replace(Some("a"));
            let selection = selections.for_account("a").unwrap();
            let sends = Arc::new(AtomicUsize::new(0));
            let held = Arc::new(tokio::sync::Notify::new());
            let send = {
                let (sends, held) = (sends.clone(), held.clone());
                async move {
                    sends.fetch_add(1, Ordering::SeqCst);
                    held.notified().await;
                    "!joined:example.org"
                }
            };
            let joining = tokio::spawn(async move {
                hand_off(&selection, true, &JoinCancellation::new(), true, send).await
            });
            while sends.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            // Sent, then the account is left while the reply is awaited.
            selections.replace(Some("b"));
            assert_eq!(joining.await.unwrap(), Err(JoinError::OutcomeUnknown));
            held.notify_waiters();
            tokio::task::yield_now().await;
            assert_eq!(sends.load(Ordering::SeqCst), 1, "a join was sent again");
        });
    }

    #[test]
    fn a_typed_room_id_is_joined_exactly_and_an_alias_takes_its_answer() {
        let asked = RoomOrAliasId::parse("!asked:example.org").unwrap();
        let same = RoomId::parse("!asked:example.org").unwrap();
        let other = RoomId::parse("!other:example.org").unwrap();
        assert_eq!(joined_room(&asked, &same).unwrap(), same);
        // Another room for a typed room ID is never taken as the joined one.
        let unexpected = joined_room(&asked, &other).unwrap_err();
        assert_eq!(unexpected, JoinError::UnexpectedRoom);
        assert!(unexpected.may_have_joined(), "it may have joined something");
        assert!(unexpected.to_string().contains("nothing was opened"));
        // An alias resolves to whatever room the homeserver names.
        let alias = RoomOrAliasId::parse("#lobby:example.org").unwrap();
        assert_eq!(joined_room(&alias, &other).unwrap(), other);
    }

    #[test]
    fn a_withdrawal_before_the_hand_off_sends_nothing_and_after_it_keeps_the_reply() {
        use crate::account_selection::Selections;
        use std::sync::atomic::AtomicUsize;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let selections = Selections::default();
            selections.replace(Some("a"));
            let selection = selections.for_account("a").unwrap();
            let sends = Arc::new(AtomicUsize::new(0));
            // Withdrawn first: nothing is sent, whatever else admits it.
            let withdrawn = JoinCancellation::new();
            assert!(withdrawn.cancel(), "withdrawn before any hand-off");
            let counted = sends.clone();
            let send = async move {
                counted.fetch_add(1, Ordering::SeqCst);
            };
            assert_eq!(
                hand_off(&selection, true, &withdrawn, true, send).await,
                Err(JoinError::Cancelled)
            );
            assert!(!withdrawn.sent());
            assert_eq!(sends.load(Ordering::SeqCst), 0);
            // Withdrawn after the hand-off: the request is not dropped, its
            // reply is kept and nothing is sent again.
            let cancellation = JoinCancellation::new();
            let held = Arc::new(Notify::new());
            let send = {
                let (sends, held) = (sends.clone(), held.clone());
                async move {
                    sends.fetch_add(1, Ordering::SeqCst);
                    held.notified().await;
                    "!joined:example.org"
                }
            };
            let joining = tokio::spawn({
                let cancellation = cancellation.clone();
                async move { hand_off(&selection, true, &cancellation, true, send).await }
            });
            while sends.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            assert!(!cancellation.cancel(), "the request had already left");
            assert!(cancellation.sent() && cancellation.is_cancelled());
            held.notify_waiters();
            assert_eq!(joining.await.unwrap(), Ok("!joined:example.org"));
            assert_eq!(sends.load(Ordering::SeqCst), 1, "a join was sent again");
            // The hand-off and a withdrawal are one choice: once withdrawn,
            // it can no longer be taken.
            let late = JoinCancellation::new();
            late.cancel();
            assert!(!late.begin_send());
            let first = JoinCancellation::new();
            assert!(first.begin_send());
            assert!(!first.begin_send(), "handed off once");
            assert!(!first.cancel());
        });
    }

    #[test]
    fn a_withdrawal_wakes_every_wait_on_it() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let cancellation = JoinCancellation::new();
            let waiting = tokio::spawn({
                let cancellation = cancellation.clone();
                async move { cancellation.cancelled().await }
            });
            tokio::task::yield_now().await;
            cancellation.cancel();
            tokio::time::timeout(Duration::from_secs(2), waiting)
                .await
                .expect("a withdrawal must end the wait")
                .unwrap();
            // Already withdrawn: the wait ends at once.
            tokio::time::timeout(Duration::from_secs(2), cancellation.cancelled())
                .await
                .expect("an earlier withdrawal is not missed");
        });
    }

    #[test]
    fn matrix_links_keep_room_event_and_routes() {
        assert_eq!(
            parse_matrix_link_target("https://matrix.to/#/%23lobby:example.org/$event:example.org?via=one.org&via=two.org").unwrap(),
            MatrixLinkTarget::Room {
                room_or_alias_id: "#lobby:example.org".into(),
                event_id: Some("$event:example.org".into()),
                via_servers: vec!["one.org".into(), "two.org".into()],
            }
        );
        assert_eq!(
            parse_matrix_link_target("matrix:roomid/abc:example.org?via=one.org").unwrap(),
            MatrixLinkTarget::Room {
                room_or_alias_id: "!abc:example.org".into(),
                event_id: None,
                via_servers: vec!["one.org".into()],
            }
        );
        assert_eq!(
            parse_matrix_link_target(" #lobby:example.org ").unwrap(),
            MatrixLinkTarget::Room {
                room_or_alias_id: "#lobby:example.org".into(),
                event_id: None,
                via_servers: vec![],
            }
        );
        assert_eq!(
            parse_matrix_link_target("@alice:example.org").unwrap(),
            MatrixLinkTarget::User {
                user_id: "@alice:example.org".into()
            }
        );
    }

    #[test]
    fn rejects_non_matrix_links() {
        for input in [
            "https://matrix.to.evil/#/!room:example.org",
            "https://example.org/",
            "$event:example.org",
            "matrix:r/not-a-room",
            "",
        ] {
            assert!(
                matches!(
                    parse_matrix_link_target(input),
                    Err(MatrixCoreError::InvalidMatrixLink(_))
                ),
                "{input}"
            );
        }
    }
}
