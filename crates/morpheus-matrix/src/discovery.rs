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
//! `disable_auth_refresh()` is root's SDK change d4c5d08, not in this base
//! (c7d0e12): this module compiles only once combined with it.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use matrix_sdk::{
    HttpError, RoomState,
    config::RequestConfig,
    ruma::{
        MatrixToUri, MatrixUri, OwnedRoomOrAliasId, OwnedServerName, RoomAliasId, RoomId,
        RoomOrAliasId, ServerName, UserId,
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

use crate::{AccountManager, AccountSelection, MatrixCoreError, Result, selected_loaded_for};

/// Routing servers one join names at most.
pub const MAX_JOIN_VIA: usize = 10;
const JOIN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_JOIN_REPLY_BYTES: u64 = 64 * 1024;
/// How long a confirmed join waits for sync to show the room joined.
const JOIN_SYNC_WAIT: Duration = Duration::from_secs(15);
const JOIN_SYNC_POLL: Duration = Duration::from_millis(250);

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
    #[error("This account cannot join rooms right now. Nothing was sent.")]
    Unavailable,
}

impl JoinError {
    /// Whether the join may have happened.
    pub fn may_have_joined(self) -> bool {
        self == Self::OutcomeUnknown
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
    /// gate), asked at the hand-off with the selection, with no await
    /// before the send. Returns the joined room's ID; nothing local changes
    /// here.
    pub async fn join_room_for(
        &self,
        selection: &AccountSelection,
        target: &MatrixLinkTarget,
        admit: impl Fn() -> bool,
    ) -> std::result::Result<String, JoinError> {
        let (id, via) = join_route(target)?;
        let client = self
            .hierarchy_client_for(selection)
            .await
            .map_err(|error| match error {
                MatrixCoreError::SelectedAccountChanged => JoinError::AccountChanged,
                _ => JoinError::Unavailable,
            })?;
        let mut request = join_room_by_id_or_alias::v3::Request::new(id);
        request.via = via;
        let sending = async { client.send(request).with_request_config(join_config()).await };
        let response = hand_off(selection, self.owns_selection(selection), admit(), sending)
            .await?
            .map_err(|error| join_failure(&error))?;
        Ok(response.room_id.to_string())
    }

    /// Wait, at most JOIN_SYNC_WAIT, for sync to show `room_id` joined for
    /// the exact `selection`: `Some(is_space)` then, `None` if it did not in
    /// time or the selection departed. Sends nothing and changes nothing.
    pub async fn await_joined_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
    ) -> Option<bool> {
        let room_id = RoomId::parse(room_id).ok()?;
        let client = self.hierarchy_client_for(selection).await.ok()?;
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
        let waited = selection
            .guard(tokio::time::timeout(JOIN_SYNC_WAIT, joined))
            .await?;
        let is_space = waited.ok()?;
        self.owns_selection(selection).then_some(is_space)
    }
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
/// unless the selection is still owned and current and `admitted` holds,
/// all checked with no await before the send. When the guard drops the
/// request, it was never sent if it had not taken its first step
/// (AccountChanged), and may have been otherwise (OutcomeUnknown). It is
/// never sent again.
pub(crate) async fn hand_off<F: Future>(
    selection: &AccountSelection,
    owned: bool,
    admitted: bool,
    send: F,
) -> std::result::Result<F::Output, JoinError> {
    if !owned || !selection.is_current() {
        return Err(JoinError::AccountChanged);
    }
    if !admitted {
        return Err(JoinError::Blocked);
    }
    let started = AtomicBool::new(false);
    let sending = async {
        started.store(true, Ordering::Release);
        send.await
    };
    selection.guard(sending).await.ok_or_else(|| {
        if started.load(Ordering::Acquire) {
            JoinError::OutcomeUnknown
        } else {
            JoinError::AccountChanged
        }
    })
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
        HttpError::Api(Box::new(FromHttpResponseError::Server(UiaaResponse::MatrixError(
            Error::new(
                reqwest::StatusCode::from_u16(status).unwrap(),
                ErrorBody::Standard(body),
            ),
        ))))
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
        let ten = (0..MAX_JOIN_VIA).map(|n| format!("s{n}.org")).collect::<Vec<_>>();
        let ten = ten.iter().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(join_route(&room(&ten)).unwrap().1.len(), MAX_JOIN_VIA);
        let mut eleven = ten.clone();
        eleven.push("s10.org");
        assert_eq!(join_route(&room(&eleven)), Err(JoinError::TooManyRoutes));
        assert_eq!(join_route(&room(&["not a server"])), Err(JoinError::InvalidTarget));
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
            assert!(failure.to_string().contains("Nothing was joined"), "{failure}");
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
                hand_off(&selection, true, false, send()).await,
                Err(JoinError::Blocked)
            );
            assert_eq!(
                hand_off(&selection, false, true, send()).await,
                Err(JoinError::AccountChanged)
            );
            assert_eq!(sends.load(Ordering::SeqCst), 0);
            // Admitted: one request, its answer kept.
            assert_eq!(
                hand_off(&selection, true, true, send()).await,
                Ok("!joined:example.org")
            );
            assert_eq!(sends.load(Ordering::SeqCst), 1);
            // The account changed before the hand-off: nothing is sent.
            selections.replace(Some("b"));
            assert_eq!(
                hand_off(&selection, true, true, send()).await,
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
            let joining =
                tokio::spawn(async move { hand_off(&selection, true, true, send).await });
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
