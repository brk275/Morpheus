//! Public room discovery and Matrix links. Directory results are untrusted
//! homeserver metadata, so the UI receives plain display fields rather than
//! SDK clients, event content, or credentials.

use matrix_sdk::ruma::{
    MatrixToUri, MatrixUri, RoomAliasId, RoomOrAliasId, ServerName, UserId,
    api::client::directory::get_public_rooms_filtered,
    directory::{Filter, PublicRoomsChunk},
    matrix_uri::MatrixId,
    room::RoomType,
};
use serde::{Deserialize, Serialize};

use crate::{AccountManager, MatrixCoreError, Result, selected_loaded_for};

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

    /// Join a parsed room or event link using its `via` servers. Holding the
    /// selection lock prevents a delayed click from joining under a different
    /// signed-in account.
    pub async fn join_matrix_room_for(
        &self,
        expected_account_id: &str,
        target: &MatrixLinkTarget,
    ) -> Result<String> {
        let MatrixLinkTarget::Room {
            room_or_alias_id,
            via_servers,
            ..
        } = target
        else {
            return Err(MatrixCoreError::InvalidMatrixLink(
                "the link identifies a user, not a room".into(),
            ));
        };
        let id = RoomOrAliasId::parse(room_or_alias_id)
            .map_err(|e| MatrixCoreError::InvalidRoomId(e.to_string()))?;
        if via_servers.len() > 10 {
            return Err(MatrixCoreError::InvalidMatrixLink(
                "too many routing servers".into(),
            ));
        }
        let via = via_servers
            .iter()
            .map(|server| {
                ServerName::parse(server)
                    .map_err(|e| MatrixCoreError::InvalidMatrixLink(e.to_string()))
            })
            .collect::<Result<Vec<_>>>()?;
        let state = self.state.lock().await;
        let client = &selected_loaded_for(&state, expected_account_id)?.client;
        let room = client
            .join_room_by_id_or_alias(&id, &via)
            .await
            .map_err(|e| MatrixCoreError::Sdk(e.to_string()))?;
        Ok(room.room_id().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
