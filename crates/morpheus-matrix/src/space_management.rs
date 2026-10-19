//! Revocable, exact-Space hierarchy browsing and child-link management.
//! Opening reads only: the server's hierarchy pages and the Space's power
//! levels. Nothing joins, previews remote rooms or fetches media. Each link
//! write re-reads that child's state, keeps fields this client does not edit,
//! is sent once without retries and is read back before it is reported.

use std::{future::Future, time::Duration};

use matrix_sdk::{
    Client, HttpError, Room, RoomState,
    config::RequestConfig,
    ruma::{
        OwnedRoomId, OwnedServerName, OwnedUserId, RoomAliasId, RoomId, ServerName, UInt,
        api::client::{
            alias::get_alias,
            room::create_room,
            space::{SpaceHierarchyRoomsChunk, get_hierarchy},
            state::send_state_event,
        },
        events::{
            InitialStateEvent, StateEventType,
            room::{
                encryption::RoomEncryptionEventContent,
                join_rules::{AllowRule, RoomJoinRulesEventContent},
            },
            space::parent::SpaceParentEventContent,
        },
        room::{JoinRuleSummary, RoomType},
        serde::Raw,
    },
};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::{
    AccountManager, AccountSelection, MatrixLinkTarget, RoomMembership, RoomSettingsCancellation,
    account_room, parse_matrix_link_target,
    room_admin::{self, MemberPower, RoomAdminError, guarded},
    room_settings::{self, READ_TIMEOUT, StateEnvelope, WRITE_TIMEOUT},
};

/// Rooms requested per hierarchy page.
pub const HIERARCHY_PAGE_LIMIT: u32 = 50;
/// Pages one session follows before it stops offering more.
pub const MAX_HIERARCHY_PAGES: usize = 20;
/// Levels below the managed Space that the server is asked to walk.
pub const HIERARCHY_MAX_DEPTH: u32 = 3;
/// Links read from one node; a larger Space is reported as truncated.
pub const MAX_CHILDREN_PER_NODE: usize = 500;
pub const MAX_VIA_SERVERS: usize = 3;
pub const MAX_ORDER_BYTES: usize = 50;
/// Links one reorder may rewrite.
pub const MAX_REORDER_WRITES: usize = 8;
/// Longest name of a room created inside a Space.
pub const MAX_CHILD_NAME_BYTES: usize = 255;
const MAX_TOKEN_BYTES: usize = 2048;
const MAX_NAME_BYTES: usize = 256;
const MAX_TOPIC_BYTES: usize = 512;
const MAX_CHILD_CONTENT_BYTES: usize = 16 * 1024;
const HIERARCHY_TIMEOUT: Duration = Duration::from_secs(20);
const ORDER_LOW: u8 = 0x20;
const ORDER_HIGH: u8 = 0x7E;

type Result<T> = std::result::Result<T, RoomAdminError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeAccess {
    Public,
    Invite,
    Knock,
    Restricted,
    KnockRestricted,
    Private,
    Other,
}

/// One room or Space returned by the account's homeserver for the hierarchy.
/// Nothing about it was fetched from a remote server or media repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceNode {
    pub room_id: String,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub canonical_alias: Option<String>,
    pub is_space: bool,
    pub joined_members: u64,
    pub access: NodeAccess,
    pub world_readable: bool,
    /// This account's local membership, or `None` when it never saw the room.
    pub membership: Option<RoomMembership>,
    /// The links this node declares, in Matrix order.
    pub children: Vec<SpaceChildLink>,
    pub children_truncated: bool,
}

/// One `m.space.child` link as the server reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceChildLink {
    pub parent_id: String,
    pub room_id: String,
    pub via: Vec<String>,
    pub suggested: bool,
    /// A valid order key. Matrix ignores an invalid key for ordering.
    pub order: Option<String>,
    pub origin_server_ts: u64,
    /// The content can be edited without losing fields or changing meaning.
    pub editable: bool,
    content: Value,
}

impl SpaceChildLink {
    /// The link a server reporting this content would describe, or `None` for
    /// a removed link. Writes still compare a link with the server's current
    /// state before changing anything.
    pub fn from_content(
        parent_id: &str,
        room_id: &str,
        origin_server_ts: u64,
        content: Value,
    ) -> Option<Self> {
        link_from_content(parent_id, room_id, origin_server_ts, content)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HierarchyPage {
    pub nodes: Vec<SpaceNode>,
    pub next_batch: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpacePermissions {
    /// The account may add, edit and remove child links now.
    pub manage_children: bool,
    /// The level needed to change child links.
    pub required_level: i64,
    pub own_power: MemberPower,
}

/// Where the route servers of a new link came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteSource {
    /// The Matrix routing algorithm over the synced members of a joined room.
    Members,
    /// The servers this account's homeserver returned for the room address.
    Address,
    /// The server hints of the Matrix link that was entered.
    Link,
}

/// A room resolved for linking. Only `resolve_target` creates one, so the
/// route servers always come from the protocol, never from free text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceTarget {
    pub room_id: String,
    pub name: Option<String>,
    pub is_space: Option<bool>,
    pub membership: Option<RoomMembership>,
    pub via: Vec<String>,
    pub route: RouteSource,
    resolved: (),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpaceWriteOutcome {
    /// Writes the homeserver accepted.
    pub accepted: usize,
    pub requested: usize,
    /// Every accepted write read back exactly as requested.
    pub confirmed: bool,
    /// Why a change of several links stopped after some were accepted.
    pub stopped: Option<RoomAdminError>,
}

/// A room `create_child` made inside the Space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedChild {
    pub room_id: String,
    /// The Space lists the new room: its link was accepted and read back.
    pub linked: bool,
    /// Why listing it failed after the room itself was created.
    pub link_error: Option<RoomAdminError>,
}

pub struct SpaceSession {
    selection: AccountSelection,
    cancellation: RoomSettingsCancellation,
    space: Room,
    own_user: OwnedUserId,
    create: StateEnvelope,
    operation: Mutex<()>,
}

impl AccountManager {
    /// Open management for one joined Space of the selected account. Opening
    /// reads the Space's create event and changes nothing.
    pub async fn open_space_management_for(
        &self,
        selection: &AccountSelection,
        space_id: &str,
        cancellation: RoomSettingsCancellation,
    ) -> Result<SpaceSession> {
        let (space, own_user) = guarded(selection, &cancellation, READ_TIMEOUT, async {
            let state = self.state.lock().await;
            let current = self
                .account_selection(selection.account_id())
                .ok_or(RoomAdminError::AccountChanged)?;
            if !selection.same_epoch(&current) {
                return Err(RoomAdminError::AccountChanged);
            }
            let space = account_room(&state, selection.account_id(), space_id)
                .map_err(|_| RoomAdminError::Unavailable)?;
            room_settings::ensure_joined(&space)?;
            if !space.is_space() {
                return Err(RoomAdminError::NotApplicable);
            }
            let own_user = space
                .client()
                .user_id()
                .ok_or(RoomAdminError::Unavailable)?
                .to_owned();
            Ok((space, own_user))
        })
        .await?;
        let create = guarded(selection, &cancellation, READ_TIMEOUT, async {
            room_settings::state(&space, StateEventType::RoomCreate, "")
                .await?
                .ok_or(RoomAdminError::InvalidState)
        })
        .await?;
        Ok(SpaceSession {
            selection: selection.clone(),
            cancellation,
            space,
            own_user,
            create,
            operation: Mutex::new(()),
        })
    }
}

impl SpaceSession {
    pub fn account_id(&self) -> &str {
        self.selection.account_id()
    }
    pub fn space_id(&self) -> &str {
        self.space.room_id().as_str()
    }
    fn check(&self) -> Result<()> {
        room_settings::ensure_scope(&self.selection, &self.cancellation)?;
        room_settings::ensure_joined(&self.space)?;
        Ok(())
    }
    async fn run<T>(&self, limit: Duration, work: impl Future<Output = Result<T>>) -> Result<T> {
        guarded(&self.selection, &self.cancellation, limit, async {
            let _operation = self.operation.lock().await;
            self.check()?;
            work.await
        })
        .await
    }

    /// The account's current right to change child links, from fresh state.
    pub async fn permissions(&self) -> Result<SpacePermissions> {
        self.run(READ_TIMEOUT, async {
            let permissions = self.read_permissions().await?;
            Ok(permissions)
        })
        .await
    }
    async fn read_permissions(&self) -> Result<SpacePermissions> {
        let power = room_settings::state(&self.space, StateEventType::RoomPowerLevels, "").await?;
        let levels = room_settings::effective_power_levels(&self.create, power.as_ref())?
            .ok_or(RoomAdminError::InvalidState)?;
        let own = &self.own_user;
        Ok(SpacePermissions {
            manage_children: levels.user_can_send_state(own, StateEventType::SpaceChild),
            required_level: levels.for_state(StateEventType::SpaceChild).into(),
            own_power: room_admin::member_power(&levels, own),
        })
    }
    async fn require_manage(&self) -> Result<()> {
        if self.read_permissions().await?.manage_children {
            Ok(())
        } else {
            Err(RoomAdminError::InsufficientPower)
        }
    }

    /// One page of the hierarchy below this Space, from this account's
    /// homeserver. Pass the previous page's `next_batch` to continue.
    pub async fn hierarchy(&self, from: Option<&str>) -> Result<HierarchyPage> {
        if from.is_some_and(|token| token.is_empty() || token.len() > MAX_TOKEN_BYTES) {
            return Err(RoomAdminError::InvalidState);
        }
        self.run(READ_TIMEOUT, async {
            let mut request = get_hierarchy::v1::Request::new(self.space.room_id().to_owned());
            request.from = from.map(str::to_owned);
            request.limit = Some(UInt::from(HIERARCHY_PAGE_LIMIT));
            request.max_depth = Some(UInt::from(HIERARCHY_MAX_DEPTH));
            let client = self.space.client();
            let response = client
                .send(request)
                .with_request_config(read_config())
                .await
                .map_err(room_admin::write_failure)?;
            self.check()?;
            let nodes = response
                .rooms
                .into_iter()
                .take(HIERARCHY_PAGE_LIMIT as usize)
                .map(|chunk| space_node(&client, chunk))
                .collect();
            let next_batch = response
                .next_batch
                .filter(|token| !token.is_empty() && token.len() <= MAX_TOKEN_BYTES);
            Ok(HierarchyPage { nodes, next_batch })
        })
        .await
    }

    /// Resolve a room ID, room address or Matrix link to a room and its route
    /// servers. Only this account's homeserver is contacted, to resolve an
    /// address; no other server is contacted and nothing is joined.
    pub async fn resolve_target(&self, input: &str) -> Result<SpaceTarget> {
        let target = parse_matrix_link_target(input).map_err(|_| RoomAdminError::InvalidRoom)?;
        let MatrixLinkTarget::Room {
            room_or_alias_id,
            via_servers,
            ..
        } = target
        else {
            return Err(RoomAdminError::InvalidRoom);
        };
        self.run(READ_TIMEOUT, async {
            let client = self.space.client();
            let (room_id, address) = if room_or_alias_id.starts_with('#') {
                let alias = RoomAliasId::parse(&room_or_alias_id)
                    .map_err(|_| RoomAdminError::InvalidRoom)?;
                let response = client
                    .send(get_alias::v3::Request::new(alias))
                    .with_request_config(read_config())
                    .await
                    .map_err(read_failure)?;
                (response.room_id, response.servers)
            } else {
                let room_id =
                    RoomId::parse(&room_or_alias_id).map_err(|_| RoomAdminError::InvalidRoom)?;
                (room_id, Vec::new())
            };
            self.check()?;
            if room_id.as_str() == self.space_id() {
                return Err(RoomAdminError::NotApplicable);
            }
            let local = client.get_room(&room_id);
            let joined = local
                .as_ref()
                .is_some_and(|room| room.state() == RoomState::Joined);
            let members = match &local {
                Some(room) if joined => room.route().await.unwrap_or_default(),
                _ => Vec::new(),
            };
            let (via, route) = choose_route(members, address, &via_servers)?;
            self.check()?;
            let name = local.as_ref().and_then(|room| room.name());
            let membership = local.as_ref().map(|room| room.state());
            Ok(SpaceTarget {
                room_id: room_id.to_string(),
                name: name.map(|name| room_admin::bounded(&name, MAX_NAME_BYTES)),
                is_space: local.as_ref().map(|room| room.is_space()),
                membership: membership.map(RoomMembership::from),
                via,
                route,
                resolved: (),
            })
        })
        .await
    }

    /// Link a resolved room below this Space. Nobody joins it, and the room
    /// stays in every other Space that links it.
    pub async fn link(
        &self,
        target: &SpaceTarget,
        suggested: bool,
        order: Option<&str>,
    ) -> Result<SpaceWriteOutcome> {
        let child = RoomId::parse(&target.room_id).map_err(|_| RoomAdminError::InvalidRoom)?;
        let order = checked_order(order)?;
        self.run(WRITE_TIMEOUT, async {
            self.require_manage().await?;
            let current = self.child_state(&child).await?;
            let content = link_content(current.as_ref(), &target.via, suggested, order)?;
            self.send_child(&child, &content).await?;
            let after = self.child_state(&child).await.ok().flatten();
            Ok(single(after.as_ref() == Some(&content)))
        })
        .await
    }

    /// Change the suggested flag or order of a direct child link and keep
    /// every other field. The reviewed link must still be the current one.
    pub async fn update(
        &self,
        link: &SpaceChildLink,
        suggested: bool,
        order: Option<&str>,
    ) -> Result<SpaceWriteOutcome> {
        let child = self.direct_child(link)?;
        let order = checked_order(order)?;
        self.run(WRITE_TIMEOUT, async {
            self.require_manage().await?;
            let current = self.child_state(&child).await?;
            if current.as_ref() != Some(&link.content) {
                return Err(RoomAdminError::TargetChanged);
            }
            let content = edited_content(&link.content, suggested, order)?;
            if content == link.content {
                return Err(RoomAdminError::NotApplicable);
            }
            self.send_child(&child, &content).await?;
            let after = self.child_state(&child).await.ok().flatten();
            Ok(single(after.as_ref() == Some(&content)))
        })
        .await
    }

    /// Remove a direct child link. The room is not left or deleted, and other
    /// Spaces that link it are unaffected; only this Space's link is cleared.
    pub async fn unlink(&self, link: &SpaceChildLink) -> Result<SpaceWriteOutcome> {
        let child = self.direct_child(link)?;
        self.run(WRITE_TIMEOUT, async {
            self.require_manage().await?;
            let current = self.child_state(&child).await?;
            if current.as_ref() != Some(&link.content) {
                return Err(RoomAdminError::TargetChanged);
            }
            self.send_child(&child, &Value::Object(Map::new())).await?;
            let after = self.child_state(&child).await;
            let removed = after.is_ok_and(|after| after.as_ref().is_none_or(|v| !is_linked(v)));
            Ok(single(removed))
        })
        .await
    }

    /// Write a reorder plan's keys one link at a time. Each reviewed link must
    /// still be current; the first refusal stops the remaining writes.
    pub async fn reorder(&self, plan: &[(SpaceChildLink, String)]) -> Result<SpaceWriteOutcome> {
        if plan.is_empty() {
            return Err(RoomAdminError::NotApplicable);
        }
        if plan.len() > MAX_REORDER_WRITES {
            return Err(RoomAdminError::TooManyChanges);
        }
        let mut children = Vec::with_capacity(plan.len());
        for (link, order) in plan {
            if !link.editable {
                return Err(RoomAdminError::UnsupportedContent);
            }
            checked_order(Some(order.as_str()))?;
            children.push(self.direct_child(link)?);
        }
        self.run(WRITE_TIMEOUT, async {
            self.require_manage().await?;
            let mut accepted = 0;
            let mut confirmed = true;
            let mut stopped = None;
            for ((link, order), child) in plan.iter().zip(&children) {
                let order = Some(order.as_str());
                let step = async {
                    let current = self.child_state(child).await?;
                    if current.as_ref() != Some(&link.content) {
                        return Err(RoomAdminError::TargetChanged);
                    }
                    let content = edited_content(&link.content, link.suggested, order)?;
                    self.send_child(child, &content).await?;
                    Ok::<_, RoomAdminError>(content)
                };
                match step.await {
                    Ok(content) => {
                        accepted += 1;
                        let after = self.child_state(child).await.ok().flatten();
                        confirmed &= after.as_ref() == Some(&content);
                    }
                    Err(error) if accepted == 0 => return Err(error),
                    Err(error) => {
                        stopped = Some(error);
                        break;
                    }
                }
            }
            Ok(SpaceWriteOutcome {
                accepted,
                requested: plan.len(),
                confirmed: confirmed && stopped.is_none(),
                stopped,
            })
        })
        .await
    }

    /// Create an encrypted room whose canonical parent is this Space, then
    /// list it here. With `members_only`, members of this Space may join it;
    /// otherwise people need an invitation. The account becomes the new
    /// room's administrator. The link's only route server is this account's
    /// homeserver, the only server that knows the room yet. Nobody else
    /// joins, and nothing else changes.
    pub async fn create_child(
        &self,
        name: &str,
        members_only: bool,
        suggested: bool,
    ) -> Result<CreatedChild> {
        let name = checked_name(name)?;
        let server = self.own_user.server_name().to_owned();
        let request = child_room_request(name, self.space.room_id(), &server, members_only);
        self.run(WRITE_TIMEOUT, async {
            self.require_manage().await?;
            self.check()?;
            let response = self
                .space
                .client()
                .send(request)
                .with_request_config(room_admin::write_config())
                .await
                .map_err(room_admin::write_failure)?;
            let child = response.room_id;
            // The room exists from here on: a failure to list it is reported
            // with the room, never hidden.
            let via = [server.to_string()];
            let linked = async {
                self.check()?;
                let current = self.child_state(&child).await?;
                let content = link_content(current.as_ref(), &via, suggested, None)?;
                self.send_child(&child, &content).await?;
                let after = self.child_state(&child).await.ok().flatten();
                Ok::<_, RoomAdminError>(after.as_ref() == Some(&content))
            };
            let (linked, link_error) = match linked.await {
                Ok(linked) => (linked, None),
                Err(error) => (false, Some(error)),
            };
            Ok(CreatedChild {
                room_id: child.to_string(),
                linked,
                link_error,
            })
        })
        .await
    }

    fn direct_child(&self, link: &SpaceChildLink) -> Result<OwnedRoomId> {
        if link.parent_id != self.space_id() {
            return Err(RoomAdminError::NotApplicable);
        }
        RoomId::parse(&link.room_id).map_err(|_| RoomAdminError::InvalidRoom)
    }
    async fn child_state(&self, child: &RoomId) -> Result<Option<Value>> {
        let kind = StateEventType::SpaceChild;
        let state = room_settings::state(&self.space, kind, child.as_str()).await?;
        Ok(state.map(|event| event.content))
    }
    async fn send_child(&self, child: &RoomId, content: &Value) -> Result<()> {
        let text = serde_json::to_string(content).map_err(|_| RoomAdminError::InvalidState)?;
        if text.len() > MAX_CHILD_CONTENT_BYTES {
            return Err(RoomAdminError::UnsupportedContent);
        }
        let raw = Raw::from_json_string(text).map_err(|_| RoomAdminError::InvalidState)?;
        let request = send_state_event::v3::Request::new_raw(
            self.space.room_id().to_owned(),
            StateEventType::SpaceChild,
            child.to_string(),
            raw,
        );
        self.check()?;
        self.space
            .client()
            .send(request)
            .with_request_config(room_admin::write_config())
            .await
            .map_err(room_admin::write_failure)?;
        self.check()
    }
}

/// A room name: trimmed, not empty, at most `MAX_CHILD_NAME_BYTES`, and free
/// of control characters.
fn checked_name(name: &str) -> Result<&str> {
    let name = name.trim();
    let valid = !name.is_empty()
        && name.len() <= MAX_CHILD_NAME_BYTES
        && !name.chars().any(char::is_control);
    valid.then_some(name).ok_or(RoomAdminError::InvalidName)
}

/// The creation request of a room inside `space`: encrypted from its first
/// event, naming `space` as its canonical parent, and joinable by the
/// Space's members when `members_only`, else by invitation.
fn child_room_request(
    name: &str,
    space: &RoomId,
    server: &ServerName,
    members_only: bool,
) -> create_room::v3::Request {
    let mut request = create_room::v3::Request::new();
    request.name = Some(name.to_owned());
    request.preset = Some(create_room::v3::RoomPreset::PrivateChat);
    let encryption = RoomEncryptionEventContent::with_recommended_defaults();
    let encryption = InitialStateEvent::with_empty_state_key(encryption);
    request.initial_state.push(encryption.to_raw_any());
    let mut parent = SpaceParentEventContent::new(vec![server.to_owned()]);
    parent.canonical = true;
    let parent = InitialStateEvent::new(space.to_owned(), parent);
    request.initial_state.push(parent.to_raw_any());
    if members_only {
        let rule = AllowRule::room_membership(space.to_owned());
        let rules = RoomJoinRulesEventContent::restricted(vec![rule]);
        let rules = InitialStateEvent::with_empty_state_key(rules);
        request.initial_state.push(rules.to_raw_any());
    }
    request
}

fn single(confirmed: bool) -> SpaceWriteOutcome {
    SpaceWriteOutcome {
        accepted: 1,
        requested: 1,
        confirmed,
        stopped: None,
    }
}

fn read_config() -> RequestConfig {
    RequestConfig::new().retry_limit(0).timeout(HIERARCHY_TIMEOUT)
}

// A fixed category only; never reflected server text. An unknown address is
// the one failure worth naming.
fn read_failure(error: HttpError) -> RoomAdminError {
    let missing = error
        .as_client_api_error()
        .is_some_and(|error| error.status_code.as_u16() == 404);
    if missing {
        RoomAdminError::RoomNotFound
    } else {
        room_admin::write_failure(error)
    }
}

fn space_node(client: &Client, chunk: SpaceHierarchyRoomsChunk) -> SpaceNode {
    let summary = chunk.summary;
    let room_id = summary.room_id.to_string();
    let membership = client
        .get_room(&summary.room_id)
        .map(|room| RoomMembership::from(room.state()));
    let children_truncated = chunk.children_state.len() > MAX_CHILDREN_PER_NODE;
    let mut children = chunk
        .children_state
        .iter()
        .take(MAX_CHILDREN_PER_NODE)
        .filter_map(|raw| parse_child_link(&room_id, raw.json().get()))
        .collect::<Vec<_>>();
    sort_links(&mut children);
    SpaceNode {
        name: summary
            .name
            .map(|name| room_admin::bounded(&name, MAX_NAME_BYTES)),
        topic: summary
            .topic
            .map(|topic| room_admin::bounded(&topic, MAX_TOPIC_BYTES)),
        canonical_alias: summary.canonical_alias.map(|alias| alias.to_string()),
        is_space: matches!(summary.room_type, Some(RoomType::Space)),
        joined_members: u64::from(summary.num_joined_members),
        access: node_access(&summary.join_rule),
        world_readable: summary.world_readable,
        membership,
        children,
        children_truncated,
        room_id,
    }
}

fn node_access(rule: &JoinRuleSummary) -> NodeAccess {
    match rule {
        JoinRuleSummary::Public => NodeAccess::Public,
        JoinRuleSummary::Invite => NodeAccess::Invite,
        JoinRuleSummary::Knock => NodeAccess::Knock,
        JoinRuleSummary::Restricted(_) => NodeAccess::Restricted,
        JoinRuleSummary::KnockRestricted(_) => NodeAccess::KnockRestricted,
        JoinRuleSummary::Private => NodeAccess::Private,
        _ => NodeAccess::Other,
    }
}

/// A valid Matrix order key: at most 50 characters from space to tilde.
pub fn valid_order(order: &str) -> bool {
    order.len() <= MAX_ORDER_BYTES
        && order
            .bytes()
            .all(|byte| (ORDER_LOW..=ORDER_HIGH).contains(&byte))
}

fn checked_order(order: Option<&str>) -> Result<Option<&str>> {
    match order {
        Some(order) if !valid_order(order) => Err(RoomAdminError::UnsupportedContent),
        order => Ok(order),
    }
}

/// A live link has a non-empty `via` list; anything else is removed.
fn is_linked(content: &Value) -> bool {
    content
        .get("via")
        .and_then(Value::as_array)
        .is_some_and(|via| !via.is_empty())
}

/// Read one stripped or full `m.space.child` event. A link without route
/// servers is a removed link and is not returned.
fn parse_child_link(parent: &str, raw: &str) -> Option<SpaceChildLink> {
    let event: Value = serde_json::from_str(raw).ok()?;
    if event.get("type")?.as_str()? != "m.space.child" {
        return None;
    }
    let room_id = RoomId::parse(event.get("state_key")?.as_str()?).ok()?;
    let origin_server_ts = event.get("origin_server_ts").and_then(Value::as_u64);
    let origin_server_ts = origin_server_ts.unwrap_or(0);
    let content = event.get("content")?.clone();
    link_from_content(parent, room_id.as_str(), origin_server_ts, content)
}

fn link_from_content(
    parent: &str,
    room_id: &str,
    origin_server_ts: u64,
    content: Value,
) -> Option<SpaceChildLink> {
    let object = content.as_object()?;
    let listed = object.get("via")?.as_array()?;
    let via = listed
        .iter()
        .filter_map(Value::as_str)
        .filter(|server| ServerName::parse(server).is_ok())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if via.is_empty() {
        return None;
    }
    let suggested = object.get("suggested");
    let order = object.get("order");
    let editable = via.len() == listed.len()
        && suggested.is_none_or(Value::is_boolean)
        && order.is_none_or(Value::is_string);
    let suggested = suggested.and_then(Value::as_bool).unwrap_or(false);
    let order = order
        .and_then(Value::as_str)
        .filter(|order| valid_order(order))
        .map(str::to_owned);
    Some(SpaceChildLink {
        parent_id: parent.to_owned(),
        room_id: room_id.to_owned(),
        via,
        suggested,
        order,
        origin_server_ts,
        editable,
        content,
    })
}

/// Matrix order: links with a valid key first, by key; then by event time
/// and room ID.
pub fn sort_links(links: &mut [SpaceChildLink]) {
    links.sort_by_cached_key(|link| {
        let unordered = link.order.is_none();
        (unordered, link.order.clone(), link.origin_server_ts, link.room_id.clone())
    });
}

/// Route servers for a new link: the routing algorithm over the synced members
/// of a joined room, else the servers returned for the room's address, else
/// the hints of the entered Matrix link. Invalid and duplicate names are
/// dropped and at most `MAX_VIA_SERVERS` remain.
fn choose_route(
    members: Vec<OwnedServerName>,
    address: Vec<OwnedServerName>,
    link: &[String],
) -> Result<(Vec<String>, RouteSource)> {
    let (servers, source): (Vec<String>, _) = if !members.is_empty() {
        let servers = members.iter().map(ToString::to_string).collect();
        (servers, RouteSource::Members)
    } else if !address.is_empty() {
        let servers = address.iter().map(ToString::to_string).collect();
        (servers, RouteSource::Address)
    } else {
        (link.to_vec(), RouteSource::Link)
    };
    let mut via = Vec::new();
    for server in servers {
        if via.len() == MAX_VIA_SERVERS {
            break;
        }
        if ServerName::parse(&server).is_ok() && !via.contains(&server) {
            via.push(server);
        }
    }
    if via.is_empty() {
        return Err(RoomAdminError::NoRoute);
    }
    Ok((via, source))
}

fn set_flags(object: &mut Map<String, Value>, suggested: bool, order: Option<&str>) {
    if suggested || object.contains_key("suggested") {
        object.insert("suggested".to_owned(), Value::Bool(suggested));
    }
    match order {
        Some(order) => {
            object.insert("order".to_owned(), Value::from(order));
        }
        None => {
            object.remove("order");
        }
    }
}

/// Content for a new link. Fields left on a removed link are kept; a live
/// link is never overwritten by adding the same room again.
fn link_content(
    existing: Option<&Value>,
    via: &[String],
    suggested: bool,
    order: Option<&str>,
) -> Result<Value> {
    let mut object = match existing {
        None => Map::new(),
        Some(content) if is_linked(content) => return Err(RoomAdminError::NotApplicable),
        Some(Value::Object(object)) => object.clone(),
        Some(_) => return Err(RoomAdminError::UnsupportedContent),
    };
    if via.is_empty() || via.len() > MAX_VIA_SERVERS {
        return Err(RoomAdminError::NoRoute);
    }
    object.insert("via".to_owned(), Value::from(via.to_vec()));
    set_flags(&mut object, suggested, order);
    Ok(Value::Object(object))
}

/// The reviewed content with only `suggested` and `order` changed. Content
/// whose fields this client cannot keep unchanged is refused, not rewritten.
fn edited_content(content: &Value, suggested: bool, order: Option<&str>) -> Result<Value> {
    let Some(link) = link_from_content("", "", 0, content.clone()) else {
        return Err(RoomAdminError::TargetChanged);
    };
    let Value::Object(object) = content else {
        return Err(RoomAdminError::UnsupportedContent);
    };
    if !link.editable {
        return Err(RoomAdminError::UnsupportedContent);
    }
    let mut object = object.clone();
    set_flags(&mut object, suggested, order);
    Ok(Value::Object(object))
}

/// The shortest order key strictly between `lower` and `upper`, using only
/// valid order characters, or `None` when no key of at most 50 characters
/// fits. A missing `upper` means "after every key".
pub fn key_between(lower: Option<&str>, upper: Option<&str>) -> Option<String> {
    let lower = lower.unwrap_or_default().as_bytes();
    let upper = upper.map(str::as_bytes);
    let valid = |key: &[u8]| key.iter().all(|byte| (ORDER_LOW..=ORDER_HIGH).contains(byte));
    if !valid(lower) || upper.is_some_and(|upper| !valid(upper) || upper <= lower) {
        return None;
    }
    let mut key = Vec::new();
    let mut tight_lower = true;
    let mut tight_upper = upper.is_some();
    while key.len() < MAX_ORDER_BYTES {
        let index = key.len();
        let low = lower.get(index).copied().filter(|_| tight_lower);
        let high = match (tight_upper, upper) {
            (true, Some(upper)) => Some(*upper.get(index)?),
            _ => None,
        };
        let min = low.map_or(ORDER_LOW, |low| low + 1);
        let max = high.map_or(ORDER_HIGH, |high| high.saturating_sub(1));
        if min <= max {
            key.push(min + (max - min) / 2);
            return String::from_utf8(key).ok();
        }
        // No digit fits here: copy the tight bound and look one place further.
        let byte = low.unwrap_or(ORDER_LOW);
        key.push(byte);
        tight_lower = tight_lower && low.is_some();
        tight_upper = tight_upper && high == Some(byte);
    }
    None
}

/// Order keys that move `links[index]` one place up or down in Matrix order
/// while changing as few links as possible. Links after the moved one that
/// have no key keep none. Fails when the move needs more than
/// `MAX_REORDER_WRITES` keys.
pub fn reorder_plan(
    links: &[SpaceChildLink],
    index: usize,
    up: bool,
) -> Result<Vec<(usize, String)>> {
    if index >= links.len() {
        return Err(RoomAdminError::NotApplicable);
    }
    let target = match up {
        true if index > 0 => index - 1,
        false if index + 1 < links.len() => index + 1,
        _ => return Err(RoomAdminError::NotApplicable),
    };
    let mut sequence = (0..links.len()).collect::<Vec<_>>();
    sequence.remove(index);
    sequence.insert(target, index);
    let mut plan = Vec::new();
    let mut previous: Option<String> = None;
    for (position, &child) in sequence.iter().enumerate() {
        let existing = links[child].order.as_deref();
        // Without a key it already sorts after every keyed link.
        if child != index && existing.is_none() && position > target {
            continue;
        }
        let above = |key: &str| previous.as_deref().is_none_or(|previous| key > previous);
        if child != index && existing.is_some_and(above) {
            previous = existing.map(str::to_owned);
            continue;
        }
        let upper = sequence[position + 1..]
            .iter()
            .filter(|&&next| next != index)
            .find_map(|&next| links[next].order.as_deref())
            .filter(|next| above(*next));
        let key = key_between(previous.as_deref(), upper)
            .or_else(|| key_between(previous.as_deref(), None))
            .ok_or(RoomAdminError::TooManyChanges)?;
        plan.push((child, key.clone()));
        if plan.len() > MAX_REORDER_WRITES {
            return Err(RoomAdminError::TooManyChanges);
        }
        previous = Some(key);
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn link(room: &str, order: Option<&str>, ts: u64) -> SpaceChildLink {
        let mut content = json!({"via": ["example.org"]});
        if let Some(order) = order {
            content["order"] = json!(order);
        }
        link_from_content("!space:example.org", room, ts, content).unwrap()
    }
    /// Apply a plan and return the rooms in their resulting Matrix order.
    fn applied(links: &[SpaceChildLink], plan: &[(usize, String)]) -> Vec<String> {
        let mut links = links.to_vec();
        for (index, key) in plan {
            links[*index].order = Some(key.clone());
        }
        sort_links(&mut links);
        links.into_iter().map(|link| link.room_id).collect()
    }
    fn rooms(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn order_keys_fall_strictly_between_bounds_with_valid_characters() {
        let cases = [
            (None, None),
            (Some("a"), Some("b")),
            (Some("a"), None),
            (None, Some("!")),
            (Some("~~~"), None),
            (Some("abc"), Some("abd")),
            (Some(""), Some("a")),
        ];
        for (lower, upper) in cases {
            let key = key_between(lower, upper).unwrap();
            assert!(valid_order(&key) && !key.is_empty());
            assert!(lower.is_none_or(|lower| key.as_str() > lower));
            assert!(upper.is_none_or(|upper| key.as_str() < upper));
        }
        for (lower, upper) in [
            (Some(""), Some(" ")),
            (Some("a"), Some("a ")),
            (Some("b"), Some("a")),
        ] {
            assert_eq!(key_between(lower, upper), None);
        }
        assert_eq!(key_between(Some("caf\u{e9}"), None), None);
        let full = "~".repeat(MAX_ORDER_BYTES);
        assert!(key_between(Some(full.as_str()), None).is_none());
    }

    #[test]
    fn reordering_moves_one_place_and_writes_only_what_it_must() {
        let keyed = [
            link("!a:example.org", Some("a"), 1),
            link("!b:example.org", Some("b"), 2),
            link("!c:example.org", Some("c"), 3),
        ];
        let plan = reorder_plan(&keyed, 2, true).unwrap();
        assert_eq!(plan.len(), 1);
        let expected = rooms(&["!a:example.org", "!c:example.org", "!b:example.org"]);
        assert_eq!(applied(&keyed, &plan), expected);

        let unkeyed = [
            link("!a:example.org", None, 1),
            link("!b:example.org", None, 2),
            link("!c:example.org", None, 3),
        ];
        let plan = reorder_plan(&unkeyed, 2, true).unwrap();
        let written = plan.iter().map(|(index, _)| *index).collect::<Vec<_>>();
        assert_eq!(written, [0, 2]);
        assert_eq!(applied(&unkeyed, &plan), expected);
        let plan = reorder_plan(&unkeyed, 0, false).unwrap();
        let down = rooms(&["!b:example.org", "!a:example.org", "!c:example.org"]);
        assert_eq!(applied(&unkeyed, &plan), down);

        let refused = Err(RoomAdminError::NotApplicable);
        assert_eq!(reorder_plan(&keyed, 0, true), refused);
        assert_eq!(reorder_plan(&keyed, 2, false), refused);
        assert_eq!(reorder_plan(&keyed, 3, true), refused);
        let long = (0..12)
            .map(|index| link(&format!("!r{index}:example.org"), None, index))
            .collect::<Vec<_>>();
        let too_many = Err(RoomAdminError::TooManyChanges);
        assert_eq!(reorder_plan(&long, 11, true), too_many);
    }

    #[test]
    fn route_servers_prefer_members_then_the_address_then_link_hints() {
        let server = |name: &str| ServerName::parse(name).unwrap();
        let members = vec![server("one.org"), server("two.org")];
        let address = vec![server("three.org")];
        let hints = rooms(&["four.org", "not a server", "four.org"]);
        let chosen = choose_route(members, address.clone(), &hints).unwrap();
        let expected = (rooms(&["one.org", "two.org"]), RouteSource::Members);
        assert_eq!(chosen, expected);
        let chosen = choose_route(Vec::new(), address, &hints).unwrap();
        assert_eq!(chosen, (rooms(&["three.org"]), RouteSource::Address));
        let chosen = choose_route(Vec::new(), Vec::new(), &hints).unwrap();
        assert_eq!(chosen, (rooms(&["four.org"]), RouteSource::Link));
        let many = rooms(&["a.org", "b.org", "c.org", "d.org"]);
        let capped = choose_route(Vec::new(), Vec::new(), &many).unwrap();
        assert_eq!(capped.0.len(), 3);
        let none = choose_route(Vec::new(), Vec::new(), &[]);
        assert_eq!(none, Err(RoomAdminError::NoRoute));
    }

    #[test]
    fn link_writes_keep_unknown_fields_and_refuse_unsupported_content() {
        let via = rooms(&["example.org"]);
        let removed = json!({"org.example.note": 1});
        let added = link_content(Some(&removed), &via, false, Some("m")).unwrap();
        let expected = json!({"org.example.note": 1, "via": ["example.org"], "order": "m"});
        assert_eq!(added, expected);
        let live = json!({"via": ["example.org"]});
        let again = link_content(Some(&live), &via, true, None);
        assert_eq!(again, Err(RoomAdminError::NotApplicable));
        let odd = json!(["not", "an", "object"]);
        let refused = link_content(Some(&odd), &via, false, None);
        assert_eq!(refused, Err(RoomAdminError::UnsupportedContent));

        let current = json!({"via": ["example.org"], "order": "a", "org.example": true});
        let edited = edited_content(&current, true, None).unwrap();
        let expected = json!({"via": ["example.org"], "suggested": true, "org.example": true});
        assert_eq!(edited, expected);
        for unsupported in [
            json!({"via": ["example.org", "not a server"]}),
            json!({"via": ["example.org"], "suggested": "yes"}),
            json!({"via": ["example.org"], "order": 7}),
        ] {
            let result = edited_content(&unsupported, false, None);
            assert_eq!(result, Err(RoomAdminError::UnsupportedContent));
        }
        let gone = edited_content(&json!({}), false, None);
        assert_eq!(gone, Err(RoomAdminError::TargetChanged));
        let long = "x".repeat(MAX_ORDER_BYTES + 1);
        assert!(checked_order(Some(long.as_str())).is_err());
    }

    #[test]
    fn child_events_parse_bounded_links_in_matrix_order() {
        let event = |room: &str, content: Value, ts: u64| {
            let mut event = json!({"type": "m.space.child", "state_key": room});
            event["origin_server_ts"] = json!(ts);
            event["content"] = content;
            event.to_string()
        };
        let parent = "!space:example.org";
        let removed = event("!gone:example.org", json!({"via": []}), 1);
        assert!(parse_child_link(parent, &removed).is_none());
        let other = event("!x:example.org", json!({"via": ["example.org"]}), 1);
        let other = other.replace("m.space.child", "m.space.parent");
        assert!(parse_child_link(parent, &other).is_none());
        let invalid_order = json!({"via": ["example.org"], "order": "\u{e9}"});
        let late = parse_child_link(parent, &event("!late:example.org", invalid_order, 9));
        let late = late.unwrap();
        assert!(late.order.is_none() && late.editable);
        let content = json!({"via": ["example.org"], "order": "z"});
        let keyed = event("!keyed:example.org", content, 5);
        let early = event("!early:example.org", json!({"via": ["example.org"]}), 2);
        let mut links = [late.clone(), parse_child_link(parent, &keyed).unwrap()]
            .into_iter()
            .chain(parse_child_link(parent, &early))
            .collect::<Vec<_>>();
        sort_links(&mut links);
        let order = links
            .iter()
            .map(|link| link.room_id.as_str())
            .collect::<Vec<_>>();
        let expected = [
            "!keyed:example.org",
            "!early:example.org",
            "!late:example.org",
        ];
        assert_eq!(order, expected);
    }

    #[test]
    fn rooms_made_inside_a_space_are_encrypted_parented_and_optionally_restricted() {
        let space = RoomId::parse("!space:example.org").unwrap();
        let server = ServerName::parse("example.org").unwrap();
        let state = |request: &create_room::v3::Request| {
            request
                .initial_state
                .iter()
                .map(|event| event.json().get().to_owned())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let open = child_room_request("Planning", &space, &server, false);
        let text = state(&open);
        assert_eq!(open.name.as_deref(), Some("Planning"));
        assert!(text.contains("m.room.encryption") && text.contains("m.megolm.v1.aes-sha2"));
        assert!(text.contains("m.space.parent") && text.contains("\"canonical\":true"));
        assert!(text.contains("!space:example.org") && !text.contains("restricted"));
        let members = state(&child_room_request("Planning", &space, &server, true));
        assert!(members.contains("restricted") && members.contains("m.room_membership"));

        assert_eq!(checked_name("  Planning  "), Ok("Planning"));
        for name in ["", "   ", "Plan\nning"] {
            assert_eq!(checked_name(name), Err(RoomAdminError::InvalidName));
        }
        let long = "x".repeat(MAX_CHILD_NAME_BYTES + 1);
        assert_eq!(checked_name(&long), Err(RoomAdminError::InvalidName));
    }

    #[test]
    fn session_and_its_network_futures_keep_the_ui_worker_send_contract() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<SpaceSession>();
        send_sync::<SpaceTarget>();
        // Compile-time proof only; no client, account, device, or service is opened.
        #[allow(dead_code)]
        fn futures(
            session: &SpaceSession,
            manager: &AccountManager,
            selection: &AccountSelection,
            target: &SpaceTarget,
            link: &SpaceChildLink,
        ) {
            fn send<T: Send>(_: T) {}
            send(manager.open_space_management_for(
                selection,
                "!space:example.org",
                RoomSettingsCancellation::new(),
            ));
            send(session.permissions());
            send(session.hierarchy(None));
            send(session.resolve_target("#room:example.org"));
            send(session.link(target, false, None));
            send(session.update(link, true, Some("a")));
            send(session.unlink(link));
            send(session.reorder(&[]));
            send(session.create_child("Planning", true, false));
        }
    }
}
