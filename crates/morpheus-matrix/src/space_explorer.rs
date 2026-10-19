//! One level of a Space's hierarchy at a time, for the read-only Space
//! explorer. A page asks the selected account's homeserver, through that
//! account's pinned client, for one Space and its direct children only
//! (`max_depth` 1): nothing below a Space the user has not opened is walked.
//! Nothing is joined, previewed from another server or fetched from a media
//! repository, and every read runs under the exact selection that asked for
//! it. Names, topics and addresses are other users' text; they come back
//! bounded, as single-line plain text.

use std::{collections::HashSet, time::Duration};

use matrix_sdk::{
    HttpError,
    ruma::{
        RoomId, UInt,
        api::{
            client::space::{SpaceHierarchyRoomsChunk, get_hierarchy},
            error::ErrorKind,
        },
    },
};

use crate::{
    AccountManager, AccountSelection, MatrixCoreError, RoomMembership, SpaceNodeInfo,
    pinned_messages::plain_text, space_node_info,
};

/// Rooms one page asks for, besides the Space itself.
pub const SPACE_LEVEL_PAGE_LIMIT: u32 = 50;
/// Child links kept from one Space; a larger list is reported as truncated.
pub const MAX_SPACE_LEVEL_LINKS: usize = 500;
/// Route servers kept for one child link.
pub const MAX_SPACE_LINK_VIA: usize = 5;
pub const MAX_SPACE_TOKEN_BYTES: usize = 4096;
pub const MAX_SPACE_NAME_CHARS: usize = 120;
pub const MAX_SPACE_TOPIC_CHARS: usize = 240;
pub const MAX_SPACE_ALIAS_CHARS: usize = 160;
const LEVEL_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_LEVEL_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpaceLevelError {
    AccountChanged,
    InvalidSpace,
    InvalidToken,
    NotFound,
    Forbidden,
    RateLimited,
    TooLarge,
    Unsupported,
    Unavailable,
}

impl SpaceLevelError {
    /// A fixed sentence: server text is never shown.
    pub fn text(self) -> &'static str {
        match self {
            Self::AccountChanged => "The account changed before this Space could be read.",
            Self::InvalidSpace => "This is not a valid Space.",
            Self::InvalidToken => "The next page of this Space could not be requested. Reload it.",
            Self::NotFound => "Your homeserver could not find this Space.",
            Self::Forbidden => "This Space can't be previewed. Join it or ask for an invitation.",
            Self::RateLimited => "Your homeserver is busy. Try again shortly.",
            Self::TooLarge => "This page of the Space is too large to show.",
            Self::Unsupported => "Your homeserver does not support browsing Spaces.",
            Self::Unavailable => "This Space could not be read. Try again.",
        }
    }
}

/// One room or Space on a level page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceLevelEntry {
    /// Bounded plain-text fields and no avatar. At most
    /// `MAX_SPACE_LEVEL_LINKS` live links, once each and never to the node
    /// itself, with at most `MAX_SPACE_LINK_VIA` validated servers each.
    pub node: SpaceNodeInfo,
    /// This account's local membership, or `None` when it never saw the room.
    pub membership: Option<RoomMembership>,
    /// The node listed more links than were kept.
    pub links_truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceLevelPage {
    pub space_id: String,
    /// The Space itself, when this page lists it; the first page does.
    pub space: Option<SpaceLevelEntry>,
    /// Every other room on the page, once each, in the server's order.
    pub entries: Vec<SpaceLevelEntry>,
    /// This account's local membership of the rooms the Space links, so a
    /// joined room the server does not describe can still be opened.
    pub linked_memberships: Vec<(String, RoomMembership)>,
    pub next_batch: Option<String>,
}

impl AccountManager {
    /// One page of a Space and its direct children for the exact selection.
    /// Pass the previous page's `next_batch` to continue the same level. The
    /// request is sent once, with bounded request and response sizes.
    pub async fn space_level_for(
        &self,
        selection: &AccountSelection,
        space_id: &str,
        from: Option<&str>,
    ) -> std::result::Result<SpaceLevelPage, SpaceLevelError> {
        let space = RoomId::parse(space_id).map_err(|_| SpaceLevelError::InvalidSpace)?;
        if from.is_some_and(|token| !valid_token(token)) {
            return Err(SpaceLevelError::InvalidToken);
        }
        let client = self
            .hierarchy_client_for(selection)
            .await
            .map_err(|error| match error {
                MatrixCoreError::SelectedAccountChanged => SpaceLevelError::AccountChanged,
                _ => SpaceLevelError::Unavailable,
            })?;
        let mut request = get_hierarchy::v1::Request::new(space.clone());
        request.from = from.map(str::to_owned);
        request.limit = Some(UInt::from(SPACE_LEVEL_PAGE_LIMIT));
        request.max_depth = Some(UInt::from(1_u32));
        let config = client
            .request_config()
            .retry_limit(0)
            .timeout(LEVEL_TIMEOUT)
            .max_response_bytes(MAX_LEVEL_RESPONSE_BYTES);
        let response = selection
            .guard(async {
                if !self.owns_selection(selection) {
                    return Err(SpaceLevelError::AccountChanged);
                }
                client
                    .send(request)
                    .with_request_config(config)
                    .await
                    .map_err(|error| level_failure(&error))
            })
            .await
            .ok_or(SpaceLevelError::AccountChanged)??;
        if !self.owns_selection(selection) {
            return Err(SpaceLevelError::AccountChanged);
        }
        let mut page = level_page(space.as_str(), response.rooms, response.next_batch);
        // Local room state only: nothing is fetched for a membership.
        let membership = |room_id: &str| {
            let room = client.get_room(&RoomId::parse(room_id).ok()?)?;
            Some(RoomMembership::from(room.state()))
        };
        for entry in page.space.iter_mut().chain(page.entries.iter_mut()) {
            entry.membership = membership(&entry.node.room_id);
        }
        if let Some(space) = &page.space {
            page.linked_memberships = space
                .node
                .children
                .iter()
                .filter_map(|link| Some((link.room_id.clone(), membership(&link.room_id)?)))
                .collect();
        }
        if !self.owns_selection(selection) {
            return Err(SpaceLevelError::AccountChanged);
        }
        Ok(page)
    }
}

pub(crate) fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_SPACE_TOKEN_BYTES
        && !token.chars().any(char::is_control)
}

/// Map one response: each room once, at most one page of rooms besides the
/// Space, and a continuation token only when it is usable.
pub(crate) fn level_page(
    space_id: &str,
    rooms: Vec<SpaceHierarchyRoomsChunk>,
    next_batch: Option<String>,
) -> SpaceLevelPage {
    let mut space = None;
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for chunk in rooms.into_iter().take(SPACE_LEVEL_PAGE_LIMIT as usize + 1) {
        let entry = level_entry(chunk);
        if !seen.insert(entry.node.room_id.clone()) {
            continue;
        }
        if entry.node.room_id == space_id {
            space = Some(entry);
        } else {
            entries.push(entry);
        }
    }
    SpaceLevelPage {
        space_id: space_id.to_owned(),
        space,
        entries,
        linked_memberships: Vec::new(),
        next_batch: next_batch.filter(|token| valid_token(token)),
    }
}

fn level_entry(mut chunk: SpaceHierarchyRoomsChunk) -> SpaceLevelEntry {
    // A longer link list is not parsed in full.
    let links_truncated = chunk.children_state.len() > MAX_SPACE_LEVEL_LINKS;
    chunk.children_state.truncate(MAX_SPACE_LEVEL_LINKS);
    let mut node = space_node_info(chunk);
    let bounded = |text: Option<String>, max_chars| {
        text.map(|text| plain_text(&text, max_chars))
            .filter(|text| !text.is_empty())
    };
    node.name = bounded(node.name, MAX_SPACE_NAME_CHARS);
    node.topic = bounded(node.topic, MAX_SPACE_TOPIC_CHARS);
    node.canonical_alias = bounded(node.canonical_alias, MAX_SPACE_ALIAS_CHARS);
    // The explorer shows no remote media; drop the address so nothing does.
    node.avatar_mxc = None;
    let mut linked = HashSet::new();
    let own_id = node.room_id.clone();
    node.children
        .retain(|link| link.room_id != own_id && linked.insert(link.room_id.clone()));
    for link in &mut node.children {
        link.via_servers.truncate(MAX_SPACE_LINK_VIA);
    }
    SpaceLevelEntry {
        node,
        membership: None,
        links_truncated,
    }
}

// A fixed category only; never reflected server text. A homeserver that
// is not in a Space answers with 403, whatever error code it uses.
fn level_failure(error: &HttpError) -> SpaceLevelError {
    if matches!(error, HttpError::ResponseTooLarge { .. }) {
        return SpaceLevelError::TooLarge;
    }
    match error.client_api_error_kind() {
        Some(ErrorKind::LimitExceeded(_)) => return SpaceLevelError::RateLimited,
        Some(ErrorKind::Unrecognized) => return SpaceLevelError::Unsupported,
        Some(ErrorKind::InvalidParam) => return SpaceLevelError::InvalidToken,
        _ => {}
    }
    match error.as_client_api_error().map(|error| error.status_code.as_u16()) {
        Some(403) => SpaceLevelError::Forbidden,
        Some(404) => SpaceLevelError::NotFound,
        _ => SpaceLevelError::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SpaceJoinRule;

    fn chunk(value: serde_json::Value) -> SpaceHierarchyRoomsChunk {
        serde_json::from_value(value).unwrap()
    }

    fn room(id: &str, children: &[&str]) -> SpaceHierarchyRoomsChunk {
        let children_state = children
            .iter()
            .enumerate()
            .map(|(index, child)| {
                serde_json::json!({
                    "type": "m.space.child", "state_key": child,
                    "sender": "@admin:example.org", "origin_server_ts": index,
                    "content": {"via": ["example.org"]}
                })
            })
            .collect::<Vec<_>>();
        chunk(serde_json::json!({
            "room_id": id, "num_joined_members": 2, "world_readable": false,
            "guest_can_join": false, "join_rule": "public", "room_type": "m.space",
            "children_state": children_state,
        }))
    }

    #[test]
    fn a_level_page_separates_the_space_and_keeps_each_room_once() {
        let rooms = vec![
            room("!space:example.org", &["!a:example.org", "!b:example.org"]),
            room("!a:example.org", &[]),
            room("!a:example.org", &[]),
            room("!b:example.org", &["!space:example.org"]),
        ];
        let page = level_page("!space:example.org", rooms, Some("next".into()));
        let space = page.space.expect("the Space itself");
        assert_eq!(space.node.children.len(), 2);
        let ids = page
            .entries
            .iter()
            .map(|entry| entry.node.room_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["!a:example.org", "!b:example.org"]);
        // A link back to the Space is kept as a link: the explorer, not the
        // core, decides that it is a cycle.
        assert_eq!(page.entries[1].node.children[0].room_id, "!space:example.org");
        assert_eq!(page.next_batch.as_deref(), Some("next"));
    }

    #[test]
    fn untrusted_fields_are_bounded_plain_text_without_media() {
        let long = "x".repeat(MAX_SPACE_NAME_CHARS * 3);
        let rooms = vec![chunk(serde_json::json!({
            "room_id": "!space:example.org", "num_joined_members": 7,
            "world_readable": false, "guest_can_join": false,
            "join_rule": "knock_restricted",
            "name": format!("Evil\u{202e}Name\n{long}"), "topic": "\u{200b}",
            "avatar_url": "mxc://example.org/abc", "room_type": "org.example.custom",
            "children_state": [
                {"type": "m.space.child", "state_key": "!space:example.org",
                 "sender": "@a:example.org", "origin_server_ts": 1,
                 "content": {"via": ["example.org"]}},
                {"type": "m.space.child", "state_key": "!c:example.org",
                 "sender": "@a:example.org", "origin_server_ts": 2,
                 "content": {"via": ["a.org", "b.org", "c.org", "d.org", "e.org", "f.org"]}},
                {"type": "m.space.child", "state_key": "!gone:example.org",
                 "sender": "@a:example.org", "origin_server_ts": 3, "content": {"via": []}}
            ],
        }))];
        let page = level_page("!space:example.org", rooms, None);
        let node = page.space.unwrap().node;
        let name = node.name.unwrap();
        assert!(name.starts_with("Evil Name x"));
        assert!(name.chars().count() <= MAX_SPACE_NAME_CHARS);
        assert!(!name.chars().any(|ch| ch.is_control() || ch == '\u{202e}'));
        assert_eq!(node.topic, None);
        assert_eq!(node.avatar_mxc, None);
        assert!(node.unsupported_type);
        assert_eq!(node.join_rule, SpaceJoinRule::KnockRestricted);
        assert_eq!(node.joined_members, 7);
        // No self-link, no removed link, bounded route servers.
        assert_eq!(node.children.len(), 1);
        assert_eq!(node.children[0].via_servers.len(), MAX_SPACE_LINK_VIA);
    }

    #[test]
    fn pages_links_and_tokens_are_bounded() {
        let ids = (0..MAX_SPACE_LEVEL_LINKS + 20)
            .map(|index| format!("!r{index}:example.org"))
            .collect::<Vec<_>>();
        let links = ids.iter().map(String::as_str).collect::<Vec<_>>();
        let mut rooms = vec![room("!space:example.org", &links)];
        rooms.extend(ids.iter().map(|id| room(id, &[])));
        let page = level_page("!space:example.org", rooms, Some("t".repeat(5000)));
        let space = page.space.unwrap();
        assert!(space.links_truncated);
        assert_eq!(space.node.children.len(), MAX_SPACE_LEVEL_LINKS);
        assert_eq!(page.entries.len(), SPACE_LEVEL_PAGE_LIMIT as usize);
        assert_eq!(page.next_batch, None);
        assert!(!valid_token(""));
        assert!(!valid_token("a\nb"));
        assert!(valid_token("s_123"));
    }
}
