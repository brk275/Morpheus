//! Matrix links clicked in messages: which ones Morpheus opens itself, and
//! what a room link names for the selected account, read without joining,
//! creating or sending anything.
//!
//! A message link is native in exactly two shapes:
//! - `https://matrix.to/#/` then a room ID, a room alias, a room and event,
//!   or a user ID (ruma's strict `MatrixToUri`: that exact scheme and host,
//!   no port, userinfo or query argument other than `via`);
//! - a `matrix:` URI (`roomid/`, `r/`, `u/`, with `e/` for an event) with no
//!   authority (`matrix://...` is refused), userinfo, port or fragment. Its
//!   `action` (join, chat) is ignored: a link never acts by itself.
//!
//! Each ID is at most MAX_LINK_ID_BYTES, an event ID starts with `$`, and a
//! link names at most MAX_JOIN_VIA valid `via` servers; the whole link is at
//! most MAX_MESSAGE_LINK_BYTES without control characters. Anything shaped
//! like one of those that fails is Unsupported: never opened natively, and a
//! `matrix:` URI is never handed to the system. Every other URL is not a
//! Matrix link and keeps the app's external-link confirmation. Nothing is
//! ever fetched from matrix.to.
//!
//! Previewing a room link (`AccountManager::preview_link_for`) reads, for
//! the exact selection:
//! - a room ID: this device's store only (membership, Space kind, name);
//! - a room alias: one typed `GET /directory/room/{alias}` to the account's
//!   own homeserver, which takes no access token, with no HTTP retry, no
//!   automatic token refresh and replay, a 15 s timeout and a 16 KiB reply.
//!   The answered room ID is typed; its server list is ignored (a join uses
//!   the link's own `via`). SDK metadata set-up before that GET (such as the
//!   server's versions) keeps the SDK's own behaviour and is not capped
//!   here. Then the store gives the membership;
//! - for an event in a joined room, its thread, from the local event cache
//!   only (no request), so the event opens in its thread.
//!
//! The client wait, the GET and the cache read are each bounded, raced
//! against the selection ending and the caller's `JoinCancellation`, and the
//! selection is checked again before an answer is returned. Errors are fixed
//! sentences without homeserver text. `disable_auth_refresh()` and
//! `max_response_bytes()` are root's SDK change d4c5d08, as for the join.

use std::time::Duration;

use matrix_sdk::{
    Client, HttpError, RoomState,
    config::RequestConfig,
    ruma::{
        EventId, OwnedRoomId, RoomAliasId, RoomId, RoomOrAliasId,
        api::{client::alias::get_alias, error::ErrorKind},
    },
};
use serde::Deserialize;
use url::Url;

use crate::{
    AccountManager, AccountSelection, JoinCancellation, MAX_JOIN_VIA, MatrixCoreError,
    MatrixLinkTarget, parse_matrix_link_target,
};

/// The longest message link handled, as the message renderer keeps them.
pub const MAX_MESSAGE_LINK_BYTES: usize = 2048;
/// The longest room ID, alias, event ID or user ID a link may name: the
/// Matrix limit for identifiers.
pub const MAX_LINK_ID_BYTES: usize = 255;
/// The longest room name a preview carries (other users' text).
pub const MAX_LINK_NAME_CHARS: usize = 256;
const MATRIX_TO: &str = "https://matrix.to/";
const ALIAS_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_ALIAS_REPLY_BYTES: u64 = 16 * 1024;
/// How long a preview waits for the account's client (the manager's lock),
/// as a join does.
const LINK_CLIENT_WAIT: Duration = Duration::from_secs(5);
const THREAD_LOOKUP_WAIT: Duration = Duration::from_secs(3);

/// What a link in a message is to Morpheus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageLink {
    /// A room, alias, event or user link Morpheus opens itself.
    Native(MatrixLinkTarget),
    /// Shaped like a Matrix link, but not one Morpheus opens. `web` is set
    /// for an `https://matrix.to/` link, which may go to the browser only
    /// through the explicit external-link confirmation.
    Unsupported { web: bool },
}

/// Classify a link from a message; `None` when it is not a Matrix link.
pub fn message_link(href: &str) -> Option<MessageLink> {
    let web = if href.starts_with(MATRIX_TO) {
        true
    } else if href.starts_with("matrix:") {
        false
    } else {
        return None;
    };
    let unsupported = Some(MessageLink::Unsupported { web });
    if href.len() > MAX_MESSAGE_LINK_BYTES || href.chars().any(char::is_control) {
        return unsupported;
    }
    if !web && !plain_matrix_uri(href) {
        return unsupported;
    }
    match parse_matrix_link_target(href) {
        Ok(target) if bounded(&target) => Some(MessageLink::Native(target)),
        _ => unsupported,
    }
}

/// A `matrix:` URI without an authority, userinfo, port or fragment.
fn plain_matrix_uri(href: &str) -> bool {
    let Ok(url) = Url::parse(href) else {
        return false;
    };
    url.scheme() == "matrix"
        && !url.has_authority()
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.fragment().is_none()
}

/// Every ID within MAX_LINK_ID_BYTES, an event ID with its sigil, and at
/// most MAX_JOIN_VIA routes, each a bounded server name.
fn bounded(target: &MatrixLinkTarget) -> bool {
    let short = |id: &str| !id.is_empty() && id.len() <= MAX_LINK_ID_BYTES;
    match target {
        MatrixLinkTarget::Room {
            room_or_alias_id,
            event_id,
            via_servers,
        } => {
            short(room_or_alias_id)
                && event_id
                    .as_deref()
                    .is_none_or(|event| short(event) && event.starts_with('$'))
                && via_servers.len() <= MAX_JOIN_VIA
                && via_servers.iter().all(|server| short(server))
        }
        MatrixLinkTarget::User { user_id } => short(user_id),
    }
}

/// A room's membership for the selected account, from this device's store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkRoom {
    Joined {
        is_space: bool,
    },
    Invited,
    Knocked,
    Left,
    Banned,
    /// Not in this device's store.
    Unknown,
}

/// What a room link names for the selected account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkPreview {
    /// The link's room ID, or the room its alias names on the homeserver.
    pub room_id: String,
    pub room: LinkRoom,
    /// The room's name in this device's store, when it has one: other
    /// users' text, bounded by MAX_LINK_NAME_CHARS.
    pub name: Option<String>,
    /// For an event in a joined room: its thread root, when the local event
    /// cache shows the event in a thread.
    pub thread_root: Option<String>,
}

/// Why a room link could not be read, as a fixed sentence: the
/// homeserver's own text is never kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("The account changed before the link was read.")]
    AccountChanged,
    #[error("Reading the link was cancelled.")]
    Cancelled,
    #[error("That link does not name a room Morpheus can open.")]
    InvalidTarget,
    #[error("Your homeserver does not know that room address.")]
    AliasNotFound,
    #[error("Your homeserver is limiting requests. Try again shortly.")]
    RateLimited,
    #[error("Your homeserver refused to look up that room address.")]
    Refused,
    #[error("No answer came about that room address.")]
    NoAnswer,
    #[error("This account cannot read links right now.")]
    Unavailable,
}

impl AccountManager {
    /// What the room link `target` names for the exact `selection` (see the
    /// module notes). Joins, creates and sends nothing; an alias costs one
    /// bounded GET to the account's own homeserver.
    pub async fn preview_link_for(
        &self,
        selection: &AccountSelection,
        target: &MatrixLinkTarget,
        cancellation: &JoinCancellation,
    ) -> Result<LinkPreview, LinkError> {
        let MatrixLinkTarget::Room {
            room_or_alias_id,
            event_id,
            ..
        } = target
        else {
            return Err(LinkError::InvalidTarget);
        };
        if !bounded(target) {
            return Err(LinkError::InvalidTarget);
        }
        let id = RoomOrAliasId::parse(room_or_alias_id).map_err(|_| LinkError::InvalidTarget)?;
        let event = match event_id {
            Some(event) => Some(EventId::parse(event).map_err(|_| LinkError::InvalidTarget)?),
            None => None,
        };
        let client = self.link_client(selection, cancellation).await?;
        let room_id = if id.is_room_id() {
            RoomId::parse(id.as_str()).map_err(|_| LinkError::InvalidTarget)?
        } else {
            let alias = RoomAliasId::parse(id.as_str()).map_err(|_| LinkError::InvalidTarget)?;
            let resolving = selection.guard(resolve_alias(&client, &alias));
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(LinkError::Cancelled),
                resolved = resolving => resolved.ok_or(LinkError::AccountChanged)??,
            }
        };
        let (room, name) = local_room(&client, &room_id);
        let thread_root = match (&event, room) {
            (Some(event), LinkRoom::Joined { .. }) => {
                let lookup = tokio::time::timeout(
                    THREAD_LOOKUP_WAIT,
                    local_thread_root(&client, &room_id, event),
                );
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(LinkError::Cancelled),
                    found = selection.guard(lookup) => {
                        found.ok_or(LinkError::AccountChanged)?.ok().flatten()
                    }
                }
            }
            _ => None,
        };
        if cancellation.is_cancelled() {
            return Err(LinkError::Cancelled);
        }
        if !self.owns_selection(selection) {
            return Err(LinkError::AccountChanged);
        }
        Ok(LinkPreview {
            room_id: room_id.to_string(),
            room,
            name,
            thread_root,
        })
    }

    /// The account's client for a link: the manager's lock is waited for at
    /// most LINK_CLIENT_WAIT, under the selection and `cancellation` (the
    /// same bound as discovery's join client; keep one at merge).
    async fn link_client(
        &self,
        selection: &AccountSelection,
        cancellation: &JoinCancellation,
    ) -> Result<Client, LinkError> {
        if cancellation.is_cancelled() {
            return Err(LinkError::Cancelled);
        }
        let acquiring =
            tokio::time::timeout(LINK_CLIENT_WAIT, self.hierarchy_client_for(selection));
        let acquired = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(LinkError::Cancelled),
            acquired = acquiring => acquired,
        };
        match acquired {
            Ok(Ok(client)) => Ok(client),
            Ok(Err(MatrixCoreError::SelectedAccountChanged)) => Err(LinkError::AccountChanged),
            // A stalled manager or an ineligible account: nothing was sent.
            Ok(Err(_)) | Err(_) => Err(LinkError::Unavailable),
        }
    }
}

/// One typed alias lookup on the account's own homeserver (see the module
/// notes): the answered room ID, typed; the server list is ignored.
async fn resolve_alias(client: &Client, alias: &RoomAliasId) -> Result<OwnedRoomId, LinkError> {
    let request = get_alias::v3::Request::new(alias.to_owned());
    match client
        .send(request)
        .with_request_config(alias_config())
        .await
    {
        Ok(response) => Ok(response.room_id),
        Err(error) => Err(alias_failure(&error)),
    }
}

/// One attempt on the wire: no HTTP retries, no automatic token refresh
/// and replay (root's d4c5d08), a timeout and a small reply.
fn alias_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .timeout(ALIAS_TIMEOUT)
        .max_response_bytes(MAX_ALIAS_REPLY_BYTES)
        .disable_auth_refresh()
}

/// A definite answer is a fixed category; anything else is no answer.
fn alias_failure(error: &HttpError) -> LinkError {
    let Some(api) = error.as_client_api_error() else {
        return LinkError::NoAnswer;
    };
    let status = api.status_code.as_u16();
    match api.error_kind() {
        Some(ErrorKind::NotFound) => LinkError::AliasNotFound,
        Some(ErrorKind::LimitExceeded(_)) => LinkError::RateLimited,
        _ if status == 404 => LinkError::AliasNotFound,
        _ if status == 429 => LinkError::RateLimited,
        _ if api.status_code.is_client_error() => LinkError::Refused,
        _ => LinkError::NoAnswer,
    }
}

/// The room's membership and name in this device's store.
fn local_room(client: &Client, room_id: &RoomId) -> (LinkRoom, Option<String>) {
    let Some(room) = client.get_room(room_id) else {
        return (LinkRoom::Unknown, None);
    };
    let membership = match room.state() {
        RoomState::Joined => LinkRoom::Joined {
            is_space: room.is_space(),
        },
        RoomState::Invited => LinkRoom::Invited,
        RoomState::Knocked => LinkRoom::Knocked,
        RoomState::Left => LinkRoom::Left,
        RoomState::Banned => LinkRoom::Banned,
    };
    let name = room
        .cached_display_name()
        .map(|name| name.to_string().chars().take(MAX_LINK_NAME_CHARS).collect());
    (membership, name)
}

/// The event's thread root from the room's local event cache: memory, then
/// this device's storage, never a request. `None` when the cache does not
/// have the event, or it is not in a thread.
async fn local_thread_root(client: &Client, room_id: &RoomId, event: &EventId) -> Option<String> {
    let room = client.get_room(room_id)?;
    let (cache, _handles) = room.event_cache().await.ok()?;
    let found = cache.find_event(event).await.ok()??;
    thread_of(found.raw().json().get())
}

/// The thread root an event's `m.relates_to` names (cleartext also on an
/// encrypted event), when its relation is a thread.
fn thread_of(event_json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Event {
        content: Content,
    }
    #[derive(Deserialize)]
    struct Content {
        #[serde(rename = "m.relates_to")]
        relates_to: Option<Relation>,
    }
    #[derive(Deserialize)]
    struct Relation {
        rel_type: Option<String>,
        event_id: Option<String>,
    }
    let relation = serde_json::from_str::<Event>(event_json)
        .ok()?
        .content
        .relates_to?;
    if relation.rel_type.as_deref() != Some("m.thread") {
        return None;
    }
    let root = EventId::parse(relation.event_id?).ok()?;
    (root.as_str().len() <= MAX_LINK_ID_BYTES).then(|| root.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        sync::{Arc, Mutex},
    };

    fn native(href: &str) -> MatrixLinkTarget {
        match message_link(href) {
            Some(MessageLink::Native(target)) => target,
            other => panic!("{href}: {other:?}"),
        }
    }

    #[test]
    fn only_exact_matrix_to_and_plain_matrix_uris_are_native() {
        assert_eq!(
            native("https://matrix.to/#/%23lobby:example.org/$event:example.org?via=one.org"),
            MatrixLinkTarget::Room {
                room_or_alias_id: "#lobby:example.org".into(),
                event_id: Some("$event:example.org".into()),
                via_servers: vec!["one.org".into()],
            }
        );
        assert_eq!(
            native("matrix:roomid/abc:example.org/e/event:example.org?via=one.org&action=join"),
            MatrixLinkTarget::Room {
                room_or_alias_id: "!abc:example.org".into(),
                event_id: Some("$event:example.org".into()),
                via_servers: vec!["one.org".into()],
            },
            "an action is ignored: a link never acts by itself"
        );
        assert_eq!(
            native("https://matrix.to/#/@alice:example.org"),
            MatrixLinkTarget::User {
                user_id: "@alice:example.org".into()
            }
        );
        // Not Matrix links at all: the external-link confirmation decides.
        for href in [
            "https://example.org/#/!room:example.org",
            "https://matrix.to.evil.example/#/!room:example.org",
            "https://www.matrix.to/#/!room:example.org",
            "http://matrix.to/#/!room:example.org",
            "https://user@matrix.to/#/!room:example.org",
            "!room:example.org",
            "#lobby:example.org",
            "javascript:alert(1)",
        ] {
            assert_eq!(message_link(href), None, "{href}");
        }
        // Matrix-shaped but not opened natively; a matrix: URI never goes to
        // the system.
        for (href, web) in [
            ("https://matrix.to/#/+group:example.org", true),
            ("https://matrix.to/#/!room:example.org?other=1", true),
            ("https://matrix.to/robots.txt", true),
            ("matrix://attacker.example/roomid/abc:example.org", false),
            ("matrix:roomid/abc:example.org#fragment", false),
            ("matrix:r/not-an-alias", false),
            ("matrix:roomid/abc:example.org\n", false),
        ] {
            assert_eq!(
                message_link(href),
                Some(MessageLink::Unsupported { web }),
                "{href}"
            );
        }
    }

    #[test]
    fn links_are_bounded_before_anything_reads_them() {
        let long = format!("!{}:example.org", "a".repeat(MAX_LINK_ID_BYTES));
        let href = format!("https://matrix.to/#/{long}");
        assert_eq!(
            message_link(&href),
            Some(MessageLink::Unsupported { web: true })
        );
        let via = (0..=MAX_JOIN_VIA)
            .map(|n| format!("via=s{n}.org"))
            .collect::<Vec<_>>()
            .join("&");
        let href = format!("https://matrix.to/#/!room:example.org?{via}");
        assert_eq!(
            message_link(&href),
            Some(MessageLink::Unsupported { web: true }),
            "more routes than a join takes"
        );
        let huge = format!(
            "https://matrix.to/#/!room:example.org?via={}",
            "a".repeat(4096)
        );
        assert_eq!(
            message_link(&huge),
            Some(MessageLink::Unsupported { web: true })
        );
        // Exactly at the bounds is still native.
        let ten = (0..MAX_JOIN_VIA)
            .map(|n| format!("via=s{n}.org"))
            .collect::<Vec<_>>()
            .join("&");
        let href = format!("https://matrix.to/#/!room:example.org?{ten}");
        assert!(matches!(message_link(&href), Some(MessageLink::Native(_))));
    }

    #[test]
    fn a_thread_root_comes_only_from_a_thread_relation() {
        let threaded = r#"{"type":"m.room.message","content":{"body":"x",
            "m.relates_to":{"rel_type":"m.thread","event_id":"$root:example.org"}}}"#;
        assert_eq!(thread_of(threaded).as_deref(), Some("$root:example.org"));
        for other in [
            r#"{"content":{"m.relates_to":{"rel_type":"m.replace","event_id":"$a:example.org"}}}"#,
            r#"{"content":{"m.relates_to":{"m.in_reply_to":{"event_id":"$a:example.org"}}}}"#,
            r#"{"content":{"m.relates_to":{"rel_type":"m.thread","event_id":"not an id"}}}"#,
            r#"{"content":{"body":"plain"}}"#,
            r#"not json"#,
        ] {
            assert_eq!(thread_of(other), None, "{other}");
        }
    }

    #[test]
    fn the_alias_lookup_is_one_capped_attempt_and_keeps_no_server_text() {
        let config = format!("{:?}", alias_config());
        assert!(config.contains("retry_limit: 0"), "{config}");
        assert!(
            config.contains(&format!("max_response_bytes: {MAX_ALIAS_REPLY_BYTES}")),
            "{config}"
        );
        assert!(config.contains("disable_auth_refresh: true"), "{config}");
        for error in [
            LinkError::AccountChanged,
            LinkError::AliasNotFound,
            LinkError::RateLimited,
            LinkError::Refused,
            LinkError::NoAnswer,
            LinkError::Unavailable,
        ] {
            assert!(!error.to_string().contains("M_"), "{error}");
        }
    }

    /// A loopback homeserver that answers `/versions` and every alias
    /// lookup with `status` and `body`, recording each request line.
    fn alias_server(status: u16, body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                        break;
                    }
                }
                let request = line.trim().to_owned();
                log.lock().unwrap().push(request.clone());
                let (status, answer) = if request.contains("/versions") {
                    (200, r#"{"versions":["v1.1","v1.5"]}"#)
                } else {
                    (status, body)
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                );
            }
        });
        (address, seen)
    }

    async fn client(homeserver: &str) -> Client {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        Client::builder()
            .http_client(http)
            .homeserver_url(homeserver)
            .build()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_alias_resolves_once_to_a_typed_room_and_ignores_its_servers() {
        let alias = RoomAliasId::parse("#lobby:example.org").unwrap();
        let answer = r#"{"room_id":"!lobby:example.org","servers":["elsewhere.example"]}"#;
        let (homeserver, seen) = alias_server(200, answer);
        let resolved = resolve_alias(&client(&homeserver).await, &alias).await;
        assert_eq!(resolved.unwrap().as_str(), "!lobby:example.org");
        let lookups = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.contains("/directory/room/"))
            .count();
        assert_eq!(lookups, 1, "one attempt, to the account's own homeserver");
        // A definite refusal is its category, with no server text kept; a
        // reply that is not a typed room ID or a server error is no answer.
        let not_found = r#"{"errcode":"M_NOT_FOUND","error":"private reflected text"}"#;
        for (status, body, expected) in [
            (404, not_found, LinkError::AliasNotFound),
            (
                403,
                r#"{"errcode":"M_FORBIDDEN","error":"x"}"#,
                LinkError::Refused,
            ),
            (
                200,
                r#"{"room_id":"not a room","servers":[]}"#,
                LinkError::NoAnswer,
            ),
            (
                502,
                r#"{"errcode":"M_UNKNOWN","error":"x"}"#,
                LinkError::NoAnswer,
            ),
        ] {
            let (homeserver, seen) = alias_server(status, body);
            let resolved = resolve_alias(&client(&homeserver).await, &alias).await;
            assert_eq!(resolved, Err(expected), "{status} {body}");
            let lookups = seen
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.contains("/directory/room/"))
                .count();
            assert_eq!(lookups, 1, "never retried: {status}");
        }
    }
}
