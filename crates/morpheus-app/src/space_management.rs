//! Pure rules for native Space management: the bounded hierarchy tree,
//! search, effect descriptions, deliberate confirmation and fixed messages.
//! The UI adapter in `ui.rs` holds the account selection, the exact Space and
//! a per-panel generation across every asynchronous result.

pub(crate) mod ui;

pub(crate) use ui::clear_presentation;

use std::collections::{HashMap, HashSet};

use morpheus_matrix::{
    CreatedChild, HIERARCHY_MAX_DEPTH, MAX_LINKS_AT_ONCE, NodeAccess, RoomAdminError,
    RoomMembership, RouteSource, SpaceChildLink, SpaceNode, SpaceTarget, SpaceWriteOutcome,
    reorder_plan, valid_order,
};

/// Rows the hierarchy view lists at once; search narrows the rest.
pub(crate) const MAX_VISIBLE_ROWS: usize = 300;
/// Rows built from the loaded pages. A room under several parents is listed
/// under each, so rows can outnumber rooms; this bounds that repetition.
const MAX_TREE_ROWS: usize = 2_000;
const MAX_QUERY_CHARS: usize = 128;
const MAX_NAME_CHARS: usize = 100;

const BAD_ORDER: &str = "Use at most 50 plain characters (letters, digits, punctuation, spaces).";
const LINK_BODY: &str = "Nobody joins it, and it stays in any other Spaces that list it.";
const UNLINK_BODY: &str =
    "Only this link is removed. Nobody leaves, the room is not deleted, and other Spaces keep it.";
const JOIN_BODY: &str = "You join only this one, not the rest of the Space. Members can see it.";
const MOVE_BODY: &str = "Everyone in the Space sees the new order.";
const CREATE_BODY: &str = "Nobody else is invited, and nothing already in the Space changes.";
pub(crate) const NOTHING_PICKED: &str = "Choose at least one room to add.";
pub(crate) const BAD_USER_ID: &str = "Enter a full Matrix user ID, like @name:example.org.";
const INVITE_BODY_START: &str = "They can join";
const INVITE_BODY_END: &str =
    "and see the rooms it lists. Each room keeps its own rules for who can join it.";
pub(crate) const NOTHING_TO_REMOVE: &str = "Choose at least one entry to remove.";
const UNLINK_MANY_BODY: &str = "Only these links are removed. Nobody leaves, no room is deleted, \
     and other Spaces keep them. They are removed one at a time, and the first failure stops \
     the rest.";
const LINK_MANY_BODY: &str = "Nobody joins them, and they stay in any other Spaces that list them. \
     They are added one at a time, and the first failure stops the rest.";
/// Rooms named in a confirmation for adding several; the rest are counted.
const MAX_NAMED_ROOMS: usize = 8;
const REPLACE_BODY: &str =
    "Nobody joins or leaves, and the old room keeps its history and stays in other Spaces.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Filter {
    All,
    Joined,
    NotJoined,
    Spaces,
    /// Links the Space suggests to its members.
    Suggested,
}

impl Filter {
    pub(crate) fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Joined,
            2 => Self::NotJoined,
            3 => Self::Spaces,
            4 => Self::Suggested,
            _ => Self::All,
        }
    }
}

/// The read in flight, which the operator may stop. Writes cannot be stopped:
/// one already handed to the homeserver may still apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reading {
    First,
    Refresh,
    More,
    Find,
    Rooms,
}

pub(crate) fn reading_text(reading: Reading) -> &'static str {
    match reading {
        Reading::First => "Loading the Space from your homeserver…",
        Reading::Refresh => "Refreshing the Space…",
        Reading::More => "Loading more of the Space…",
        Reading::Find => "Finding the room through your homeserver…",
        Reading::Rooms => "Listing the rooms you have joined…",
    }
}

pub(crate) fn stopped_text(reading: Reading) -> &'static str {
    match reading {
        Reading::First => "Stopped loading the Space. Refresh to try again.",
        Reading::Refresh => "Stopped refreshing. The list may be out of date.",
        Reading::More => "Stopped loading more. The entries already shown are kept.",
        Reading::Find => "Stopped finding the room. Nothing was added.",
        Reading::Rooms => "Stopped listing your rooms. Nothing was added.",
    }
}

/// One link in the flattened hierarchy below the managed Space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) link: SpaceChildLink,
    /// Index of the linked room in the loaded nodes, or `None` when the
    /// server has not returned it (not loaded yet, or not visible to you).
    pub(crate) node: Option<usize>,
    /// 1 for a link of the managed Space itself.
    pub(crate) depth: usize,
    /// Already listed through another parent, so it is not expanded again.
    pub(crate) repeated: bool,
}

impl Row {
    /// Unique per link: a room can be linked from several parents.
    pub(crate) fn key(&self) -> String {
        format!("{}\n{}", self.link.parent_id, self.link.room_id)
    }
    pub(crate) fn direct(&self) -> bool {
        self.depth == 1
    }
}

/// Flatten the loaded hierarchy depth-first in Matrix order. A room linked
/// from several parents is listed under each but expanded once, which also
/// stops cycles; depth is capped like the server request.
pub(crate) fn rows(space_id: &str, nodes: &[SpaceNode]) -> Vec<Row> {
    let index = nodes
        .iter()
        .enumerate()
        .map(|(position, node)| (node.room_id.as_str(), position))
        .collect::<HashMap<_, _>>();
    let Some(&root) = index.get(space_id) else {
        return Vec::new();
    };
    let mut expanded = HashSet::from([space_id]);
    let mut stack = nodes[root]
        .children
        .iter()
        .rev()
        .map(|link| (link, 1))
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    while let Some((link, depth)) = stack.pop() {
        if rows.len() == MAX_TREE_ROWS {
            break;
        }
        let node = index.get(link.room_id.as_str()).copied();
        let repeated = !expanded.insert(link.room_id.as_str());
        if let Some(node) = node
            && !repeated
            && depth < HIERARCHY_MAX_DEPTH as usize
        {
            let children = nodes[node].children.iter().rev();
            stack.extend(children.map(|child| (child, depth + 1)));
        }
        rows.push(Row {
            link: link.clone(),
            node,
            depth,
            repeated,
        });
    }
    rows
}

/// Rows matching a case-insensitive search of names, addresses and IDs, at
/// most `MAX_VISIBLE_ROWS`, with the full match count.
pub(crate) fn visible<'a>(
    rows: &'a [Row],
    nodes: &[SpaceNode],
    query: &str,
    filter: Filter,
) -> (Vec<&'a Row>, usize) {
    let needle = query
        .trim()
        .chars()
        .take(MAX_QUERY_CHARS)
        .collect::<String>()
        .to_lowercase();
    let mut shown = Vec::new();
    let mut total = 0;
    for row in rows {
        let node = row.node.map(|index| &nodes[index]);
        if !admits(filter, row, node) || !matches_query(&row.link.room_id, node, &needle) {
            continue;
        }
        total += 1;
        if shown.len() < MAX_VISIBLE_ROWS {
            shown.push(row);
        }
    }
    (shown, total)
}

fn admits(filter: Filter, row: &Row, node: Option<&SpaceNode>) -> bool {
    let joined = node.is_some_and(|node| node.membership == Some(RoomMembership::Joined));
    match filter {
        Filter::All => true,
        Filter::Joined => joined,
        Filter::NotJoined => !joined,
        Filter::Spaces => node.is_some_and(|node| node.is_space),
        Filter::Suggested => row.link.suggested,
    }
}

fn matches_query(room_id: &str, node: Option<&SpaceNode>, needle: &str) -> bool {
    if needle.is_empty() || room_id.to_lowercase().contains(needle) {
        return true;
    }
    let Some(node) = node else {
        return false;
    };
    let name = node.name.as_deref().unwrap_or_default();
    let alias = node.canonical_alias.as_deref().unwrap_or_default();
    name.to_lowercase().contains(needle) || alias.to_lowercase().contains(needle)
}

/// A sanitized title: the room's name, else its address, else its ID.
pub(crate) fn title(room_id: &str, node: Option<&SpaceNode>) -> String {
    let named = node.and_then(|node| node.name.as_deref().or(node.canonical_alias.as_deref()));
    let text = crate::directory_plain_text(named.unwrap_or(room_id), MAX_NAME_CHARS);
    if text.is_empty() {
        crate::directory_plain_text(room_id, MAX_NAME_CHARS)
    } else {
        text
    }
}

pub(crate) fn membership_text(membership: Option<RoomMembership>) -> &'static str {
    match membership {
        Some(RoomMembership::Joined) => "Joined",
        Some(RoomMembership::Invited) => "Invited",
        Some(RoomMembership::Knocked) => "Asked to join",
        Some(RoomMembership::Left) => "Left",
        Some(RoomMembership::Banned) => "Banned",
        None => "Not joined",
    }
}

fn access_text(access: NodeAccess) -> &'static str {
    match access {
        NodeAccess::Public => "Public",
        NodeAccess::Invite => "Invite only",
        NodeAccess::Knock => "Ask to join",
        NodeAccess::Restricted => "Space members",
        NodeAccess::KnockRestricted => "Space members or ask to join",
        NodeAccess::Private => "Private",
        NodeAccess::Other => "Custom access",
    }
}

/// The secondary line of a hierarchy row.
pub(crate) fn row_detail(row: &Row, nodes: &[SpaceNode]) -> String {
    let mut parts = Vec::new();
    match row.node.map(|index| &nodes[index]) {
        Some(node) => {
            let kind = if node.is_space { "Space" } else { "Room" };
            parts.push(kind.to_owned());
            parts.push(format!("{} members", node.joined_members));
            parts.push(membership_text(node.membership).to_owned());
            parts.push(access_text(node.access).to_owned());
        }
        None => parts.push("Not returned by your homeserver yet".to_owned()),
    }
    if row.link.suggested {
        parts.push("Suggested".to_owned());
    }
    if row.repeated {
        parts.push("Also listed above".to_owned());
    }
    if row.link.replaced_by.is_some() {
        parts.push("Replaced by a newer room".to_owned());
    }
    parts.join(" · ")
}

/// The selected row's full description, including what can be done.
pub(crate) fn selected_detail(row: &Row, nodes: &[SpaceNode], can_manage: bool) -> String {
    let mut text = row_detail(row, nodes);
    let node = row.node.map(|index| &nodes[index]);
    if let Some(topic) = node.and_then(|node| node.topic.as_deref()) {
        text.push_str("\n\n");
        text.push_str(&crate::directory_plain_text(topic, 240));
    }
    let order = row.link.order.as_deref().unwrap_or("none");
    let order = crate::directory_plain_text(order, 60);
    text.push_str(&format!("\n\nOrder key: {order}. Linked by "));
    if row.direct() {
        text.push_str("this Space.");
    } else {
        let parent = nodes.iter().find(|node| node.room_id == row.link.parent_id);
        text.push_str(&title(&row.link.parent_id, parent));
        text.push_str(". Open that Space to change this link.");
    }
    if row.direct() && !row.link.editable {
        text.push_str(" This link uses content Morpheus will not rewrite.");
    } else if row.direct() && !can_manage {
        text.push_str(" Your level does not allow changing links.");
    }
    if let Some(replacement) = &row.link.replaced_by {
        let id = crate::directory_plain_text(replacement, 260);
        text.push_str(&format!("\n\nThis room was upgraded. Its replacement is {id}."));
    }
    text
}

/// A joined Space below the managed one, which can be shown here to manage
/// its own links: its room ID and title.
pub(crate) fn openable_space(
    row: &Row,
    nodes: &[SpaceNode],
    space_id: &str,
) -> Option<(String, String)> {
    let node = &nodes[row.node?];
    let joined = node.membership == Some(RoomMembership::Joined);
    if !node.is_space || !joined || node.room_id == space_id {
        return None;
    }
    Some((node.room_id.clone(), title(&node.room_id, Some(node))))
}

/// An empty or whitespace-only field means no order key.
pub(crate) fn parse_order(input: &str) -> Result<Option<String>, String> {
    if input.trim().is_empty() {
        return Ok(None);
    }
    if !valid_order(input) {
        return Err(BAD_ORDER.to_owned());
    }
    Ok(Some(input.to_owned()))
}

fn route_text(target: &SpaceTarget) -> String {
    let servers = target.via.join(", ");
    let source = match target.route {
        RouteSource::Members => "from the room's members",
        RouteSource::Address => "from the room address",
        RouteSource::Link => "from the link",
    };
    crate::directory_plain_text(&format!("Route servers {source}: {servers}."), 240)
}

/// The resolved room awaiting the operator's choices, as one description.
pub(crate) fn target_text(target: &SpaceTarget) -> String {
    let name = title(&target.room_id, None);
    let name = target
        .name
        .as_deref()
        .map(|name| crate::directory_plain_text(name, MAX_NAME_CHARS))
        .filter(|name| !name.is_empty())
        .unwrap_or(name);
    let kind = match target.is_space {
        Some(true) => "Space",
        Some(false) => "Room",
        None => "Room or Space",
    };
    let membership = membership_text(target.membership);
    let room = crate::directory_plain_text(&target.room_id, 260);
    let route = route_text(target);
    format!("{name}\n{kind} · {membership} · {room}\n{route}")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    Link {
        target: SpaceTarget,
        suggested: bool,
        order: Option<String>,
    },
    /// Add several of the account's joined rooms, in the listed order.
    LinkMany {
        targets: Vec<SpaceTarget>,
        suggested: bool,
    },
    Update {
        link: SpaceChildLink,
        suggested: bool,
        order: Option<String>,
    },
    Unlink { link: SpaceChildLink },
    /// Remove several direct entries, in the listed order.
    UnlinkMany { links: Vec<SpaceChildLink> },
    /// List the room that replaced an upgraded child in its place.
    Replace {
        link: SpaceChildLink,
        replacement: String,
    },
    Reorder {
        plan: Vec<(SpaceChildLink, String)>,
        up: bool,
    },
    Join {
        room_id: String,
        via: Vec<String>,
        is_space: bool,
    },
    /// Invite one person to the Space itself.
    Invite { user_id: String },
    /// Create a new room, or a sub-Space, inside the Space and list it there.
    Create {
        name: String,
        is_space: bool,
        members_only: bool,
        suggested: bool,
    },
}

/// A change the operator reviewed, with the names it was reviewed under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Intent {
    pub(crate) change: Change,
    pub(crate) name: String,
    pub(crate) space: String,
}

impl Intent {
    /// The room the change concerns, for row markers and stale checks.
    pub(crate) fn room_id(&self) -> &str {
        match &self.change {
            Change::Link { target, .. } => target.room_id.as_str(),
            Change::Update { link, .. }
            | Change::Unlink { link }
            | Change::Replace { link, .. } => link.room_id.as_str(),
            Change::Reorder { plan, .. } => {
                plan.first().map_or("", |(link, _)| link.room_id.as_str())
            }
            Change::Join { room_id, .. } => room_id.as_str(),
            // New entries are not rows yet, and several entries mark none.
            Change::Create { .. } | Change::LinkMany { .. } | Change::UnlinkMany { .. } => "",
            Change::Invite { .. } => "",
        }
    }
    /// Every reviewed link is still listed unchanged by a fresher load.
    pub(crate) fn still_matches(&self, direct: &[SpaceChildLink]) -> bool {
        let listed = |link: &SpaceChildLink| direct.iter().any(|item| item == link);
        match &self.change {
            Change::Update { link, .. }
            | Change::Unlink { link }
            | Change::Replace { link, .. } => listed(link),
            Change::Reorder { plan, .. } => plan.iter().all(|(link, _)| listed(link)),
            Change::UnlinkMany { links } => links.iter().all(listed),
            Change::Link { .. }
            | Change::LinkMany { .. }
            | Change::Join { .. }
            | Change::Invite { .. }
            | Change::Create { .. } => true,
        }
    }
}

/// The reorder that moves one direct child a place up or down.
pub(crate) fn move_change(
    direct: &[SpaceChildLink],
    room_id: &str,
    up: bool,
) -> Result<Change, String> {
    let Some(index) = direct.iter().position(|link| link.room_id == room_id) else {
        return Err("This room is not a direct entry of the Space.".to_owned());
    };
    let plan = reorder_plan(direct, index, up).map_err(error_text)?;
    if plan.iter().any(|(position, _)| !direct[*position].editable) {
        return Err(error_text(RoomAdminError::UnsupportedContent));
    }
    let plan = plan
        .into_iter()
        .map(|(position, key)| (direct[position].clone(), key))
        .collect();
    Ok(Change::Reorder { plan, up })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Confirmation {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) action: &'static str,
    pub(crate) destructive: bool,
}

/// Every change is reviewed with its effects before anything is sent.
pub(crate) fn confirmation(intent: &Intent) -> Confirmation {
    let (name, space) = (&intent.name, &intent.space);
    match &intent.change {
        Change::Link {
            target,
            suggested,
            order,
        } => {
            let mut body = format!("Members of {space} will see {name} listed. {LINK_BODY}");
            if *suggested {
                body.push_str(" It is suggested to members.");
            }
            if let Some(order) = order {
                body.push_str(&format!(" Order key: {order}."));
            }
            body.push(' ');
            body.push_str(&route_text(target));
            Confirmation {
                title: format!("Add {name} to {space}?"),
                body,
                action: "Add",
                destructive: false,
            }
        }
        Change::LinkMany { targets, suggested } => {
            let named = targets.iter().take(MAX_NAMED_ROOMS).map(candidate_title);
            let named = named.collect::<Vec<_>>().join(", ");
            let mut body = format!("Members of {space} will see {named}");
            let more = targets.len().saturating_sub(MAX_NAMED_ROOMS);
            if more > 0 {
                body.push_str(&format!(" and {more} more"));
            }
            body.push_str(&format!(" listed. {LINK_MANY_BODY}"));
            if *suggested {
                body.push_str(" They are suggested to members.");
            }
            body.push_str(" Route servers come from each room's members.");
            Confirmation {
                title: format!("Add {name} to {space}?"),
                body,
                action: "Add",
                destructive: false,
            }
        }
        Change::Update {
            link,
            suggested,
            order,
        } => {
            let flag = |on: bool| if on { "suggested" } else { "not suggested" };
            let key = |order: Option<&str>| order.unwrap_or("none").to_owned();
            let body = format!(
                "From {} to {}; order key from {} to {}. Everyone in {space} sees the change.",
                flag(link.suggested),
                flag(*suggested),
                key(link.order.as_deref()),
                key(order.as_deref())
            );
            Confirmation {
                title: format!("Update {name} in {space}?"),
                body,
                action: "Update",
                destructive: false,
            }
        }
        Change::Unlink { .. } => Confirmation {
            title: format!("Remove {name} from {space}?"),
            body: UNLINK_BODY.to_owned(),
            action: "Remove link",
            destructive: true,
        },
        Change::UnlinkMany { links } => {
            let named = links.iter().take(MAX_NAMED_ROOMS);
            let named = named.map(|link| title(&link.room_id, None));
            let named = named.collect::<Vec<_>>().join(", ");
            let mut body = format!("Members of {space} will no longer see {named}");
            let more = links.len().saturating_sub(MAX_NAMED_ROOMS);
            if more > 0 {
                body.push_str(&format!(" and {more} more"));
            }
            body.push_str(&format!(" listed. {UNLINK_MANY_BODY}"));
            Confirmation {
                title: format!("Remove {name} from {space}?"),
                body,
                action: "Remove links",
                destructive: true,
            }
        }
        Change::Replace { link, replacement } => {
            let id = crate::directory_plain_text(replacement, 260);
            let suggested = if link.suggested {
                "suggested"
            } else {
                "not suggested"
            };
            let order = link.order.as_deref().unwrap_or("none");
            let order = crate::directory_plain_text(order, 60);
            Confirmation {
                title: format!("List the new room for {name}?"),
                body: format!(
                    "{name} was upgraded to {id}. {space} lists the new room in its place \
                     ({suggested}, order key {order}), then removes the link to the old room. \
                     {REPLACE_BODY}"
                ),
                action: "Use new room",
                destructive: false,
            }
        }
        Change::Reorder { plan, up } => {
            let direction = if *up { "up" } else { "down" };
            let count = plan.len();
            let entries = if count == 1 { "entry gets" } else { "entries get" };
            Confirmation {
                title: format!("Move {name} {direction}?"),
                body: format!("{count} {entries} a new order key. {MOVE_BODY}"),
                action: "Move",
                destructive: false,
            }
        }
        Change::Invite { .. } => Confirmation {
            title: format!("Invite {name} to {space}?"),
            body: format!("{INVITE_BODY_START} {space} {INVITE_BODY_END}"),
            action: "Invite",
            destructive: false,
        },
        Change::Join { is_space, .. } => {
            let kind = if *is_space { "Space" } else { "room" };
            Confirmation {
                title: format!("Join {name}?"),
                body: format!("This joins the {kind} {name}. {JOIN_BODY}"),
                action: "Join",
                destructive: false,
            }
        }
        Change::Create {
            is_space,
            members_only,
            suggested,
            ..
        } => {
            let who = if *members_only {
                format!("Members of {space} can join it")
            } else {
                "People need an invitation to join it".to_owned()
            };
            let what = if *is_space {
                "a new Space"
            } else {
                "a new encrypted room"
            };
            let mut body = format!(
                "Morpheus creates {what} and lists it in {space}. {who}, and you are its \
                 administrator. {CREATE_BODY}"
            );
            if *suggested {
                body.push_str(" It is suggested to members.");
            }
            Confirmation {
                title: format!("Create {name} in {space}?"),
                body,
                action: "Create room",
                destructive: false,
            }
        }
    }
}

pub(crate) fn pending_text(intent: &Intent) -> String {
    let verb = match intent.change {
        Change::Link { .. } | Change::LinkMany { .. } => "Adding",
        Change::Update { .. } => "Updating",
        Change::Unlink { .. } | Change::UnlinkMany { .. } => "Removing",
        Change::Replace { .. } => "Replacing",
        Change::Reorder { .. } => "Moving",
        Change::Join { .. } => "Joining",
        Change::Invite { .. } => "Inviting",
        Change::Create { .. } => "Creating",
    };
    format!("{verb} {}… Waiting for the homeserver.", intent.name)
}

/// Report only what the homeserver accepted and what reading it back showed.
pub(crate) fn outcome_text(intent: &Intent, outcome: SpaceWriteOutcome) -> String {
    let name = &intent.name;
    if let Change::Replace { .. } = intent.change {
        return replaced_text(name, &intent.space, outcome);
    }
    if let Change::LinkMany { .. } = intent.change {
        return linked_text(name, &intent.space, outcome);
    }
    if let Change::UnlinkMany { .. } = intent.change {
        return unlinked_text(name, &intent.space, outcome);
    }
    if let Some(error) = outcome.stopped {
        let (accepted, requested) = (outcome.accepted, outcome.requested);
        let reason = error_text(error);
        return format!("{accepted} of {requested} order keys were saved, then: {reason}");
    }
    if !outcome.confirmed {
        return format!(
            "The homeserver accepted the change for {name}, but reading it back differed."
        );
    }
    match intent.change {
        Change::Link { .. } => format!("Added {name}. The homeserver confirmed the link."),
        Change::LinkMany { .. } => linked_text(name, &intent.space, outcome),
        Change::Update { .. } => format!("Updated {name}. The homeserver confirmed it."),
        Change::Unlink { .. } => format!("Removed the link to {name}; the room is unchanged."),
        Change::UnlinkMany { .. } => unlinked_text(name, &intent.space, outcome),
        Change::Replace { .. } => replaced_text(name, &intent.space, outcome),
        Change::Reorder { .. } => format!("Moved {name}. The homeserver confirmed the order."),
        Change::Join { .. } => format!("Joined {name}."),
        Change::Invite { .. } => {
            let space = &intent.space;
            format!("Invited {name} to {space}. The homeserver confirmed it.")
        }
        Change::Create { .. } => format!("Created {name}."),
    }
}

/// What adding several rooms did: each was added in order until the first
/// failure, which is named.
fn linked_text(name: &str, space: &str, outcome: SpaceWriteOutcome) -> String {
    let (accepted, requested) = (outcome.accepted, outcome.requested);
    if let Some(error) = outcome.stopped {
        let reason = error_text(error);
        return format!("{accepted} of {requested} rooms were added to {space}, then: {reason}");
    }
    if !outcome.confirmed {
        return format!(
            "The homeserver accepted the links for {name}, but reading them back differed."
        );
    }
    format!("Added {name} to {space}. The homeserver confirmed each link.")
}

pub(crate) fn too_many_picked() -> String {
    format!("Add at most {MAX_LINKS_AT_ONCE} rooms at a time.")
}

/// What removing several entries did: each was removed in order until the
/// first failure, which is named.
fn unlinked_text(name: &str, space: &str, outcome: SpaceWriteOutcome) -> String {
    let (accepted, requested) = (outcome.accepted, outcome.requested);
    if let Some(error) = outcome.stopped {
        let reason = error_text(error);
        return format!(
            "{accepted} of {requested} entries were removed from {space}, then: {reason}"
        );
    }
    if !outcome.confirmed {
        return format!(
            "The homeserver accepted removing {name}, but reading the links back differed."
        );
    }
    format!("Removed {name} from {space}; the rooms themselves are unchanged.")
}

/// A full Matrix user ID as typed, trimmed; the session parses it exactly.
pub(crate) fn parse_user_id(input: &str) -> Result<String, String> {
    let user_id = input.trim();
    let shaped = user_id.len() <= 255
        && user_id.starts_with('@')
        && user_id.contains(':')
        && !user_id.chars().any(char::is_whitespace)
        && !user_id.chars().any(char::is_control);
    if shaped {
        Ok(user_id.to_owned())
    } else {
        Err(BAD_USER_ID.to_owned())
    }
}

/// "1 entry" or "N entries", for a removal of several.
pub(crate) fn entries_text(count: usize) -> String {
    if count == 1 {
        "1 entry".to_owned()
    } else {
        format!("{count} entries")
    }
}

/// The secondary line of a direct entry offered for removal.
pub(crate) fn entry_detail(link: &SpaceChildLink, node: Option<&SpaceNode>) -> String {
    let mut parts = Vec::new();
    match node {
        Some(node) => {
            parts.push(if node.is_space { "Space" } else { "Room" });
            parts.push(membership_text(node.membership));
        }
        None => parts.push("Not returned by your homeserver yet"),
    }
    if link.suggested {
        parts.push("Suggested");
    }
    parts.join(" · ")
}

/// Direct entries Morpheus can remove: their content is one it edits.
pub(crate) fn removable(direct: &[SpaceChildLink]) -> Vec<&SpaceChildLink> {
    direct.iter().filter(|link| link.editable).collect()
}

/// "1 room" or "N rooms", for a change of several.
pub(crate) fn rooms_text(count: usize) -> String {
    if count == 1 {
        "1 room".to_owned()
    } else {
        format!("{count} rooms")
    }
}

/// A joined room offered for adding: its sanitized name, else its ID.
pub(crate) fn candidate_title(target: &SpaceTarget) -> String {
    let id = crate::directory_plain_text(&target.room_id, MAX_NAME_CHARS);
    let name = target.name.as_deref();
    let name = name.map(|name| crate::directory_plain_text(name, MAX_NAME_CHARS));
    name.filter(|name| !name.is_empty()).unwrap_or(id)
}

/// The secondary line of a joined room offered for adding.
pub(crate) fn candidate_detail(target: &SpaceTarget) -> String {
    let kind = if target.is_space == Some(true) {
        "Space"
    } else {
        "Room"
    };
    let servers = crate::directory_plain_text(&target.via.join(", "), 120);
    format!("{kind} · via {servers}")
}

/// Whether a joined room matches a case-insensitive search of its name and
/// ID; an empty search matches every room.
pub(crate) fn candidate_matches(target: &SpaceTarget, query: &str) -> bool {
    let needle = query
        .trim()
        .chars()
        .take(MAX_QUERY_CHARS)
        .collect::<String>()
        .to_lowercase();
    let title = candidate_title(target).to_lowercase();
    needle.is_empty() || title.contains(&needle) || target.room_id.to_lowercase().contains(&needle)
}

/// Joined rooms the Space does not list directly yet, in the given order.
pub(crate) fn candidates<'a>(
    rooms: &'a [SpaceTarget],
    direct: &[SpaceChildLink],
) -> Vec<&'a SpaceTarget> {
    let listed = direct
        .iter()
        .map(|link| link.room_id.as_str())
        .collect::<HashSet<_>>();
    rooms
        .iter()
        .filter(|room| !listed.contains(room.room_id.as_str()))
        .collect()
}

/// What replacing an upgraded room did: the new room is listed first, so a
/// failure after that leaves both listed rather than neither.
fn replaced_text(name: &str, space: &str, outcome: SpaceWriteOutcome) -> String {
    if let Some(error) = outcome.stopped {
        let reason = error_text(error);
        return format!(
            "{space} now lists the new room for {name}, but removing the old link failed: \
             {reason}"
        );
    }
    if !outcome.confirmed {
        return format!(
            "The homeserver accepted the changes for {name}, but reading them back differed."
        );
    }
    format!("{space} now lists the new room for {name}. The homeserver confirmed it.")
}

/// The notice and error after a room was created inside the Space: listing
/// it can fail after the room itself exists, and then says so.
pub(crate) fn created_text(intent: &Intent, created: &CreatedChild) -> (String, String) {
    let (name, space) = (&intent.name, &intent.space);
    if created.linked {
        return (format!("Created {name} and listed it in {space}."), String::new());
    }
    let id = crate::directory_plain_text(&created.room_id, 260);
    match created.link_error {
        Some(error) => (
            format!("Created {name} ({id})."),
            format!(
                "Listing it in {space} failed: {} Add it with Find using its ID.",
                error_text(error)
            ),
        ),
        None => (
            format!("Created {name}; its listing was accepted, but reading it back differed."),
            String::new(),
        ),
    }
}

pub(crate) fn permission_text(can_manage: bool, required: i64, own: &str) -> String {
    if can_manage {
        format!("You can change this Space's entries (level {required} needed; you have {own}).")
    } else {
        format!("Changing entries needs level {required}; you have {own}. You can still browse.")
    }
}

pub(crate) fn summary(rows: &[Row], shown: usize, total: usize, has_more: bool) -> String {
    let direct = rows.iter().filter(|row| row.direct()).count();
    let mut text = format!("{direct} direct entries, {} listed", rows.len());
    if total > shown {
        text.push_str(&format!(" · showing {shown} of {total} matches"));
    }
    if has_more {
        text.push_str(" · more can be loaded");
    }
    text
}

/// Fixed, actionable text for one failure; server text is never shown.
pub(crate) fn error_text(error: RoomAdminError) -> String {
    let text = match error {
        RoomAdminError::Cancelled | RoomAdminError::AccountChanged => {
            "The Space view closed before the change finished. Open it again."
        }
        RoomAdminError::NotJoined => "Join this Space to manage it.",
        RoomAdminError::Unavailable | RoomAdminError::TimedOut => {
            "The homeserver did not answer. A change may still apply; refresh before retrying."
        }
        RoomAdminError::RateLimited => "The homeserver is limiting requests. Wait, then refresh.",
        RoomAdminError::InvalidState => "The Space returned state Morpheus cannot read safely.",
        RoomAdminError::Forbidden => "The homeserver refused this. Your level may have changed.",
        RoomAdminError::InsufficientPower => "Your current level does not allow changing entries.",
        RoomAdminError::TargetChanged => "This entry changed on the server. Review it again.",
        RoomAdminError::NotApplicable => "This change does not apply to the entry as it is now.",
        RoomAdminError::InvalidRoom => "Enter a room ID, room address or Matrix link.",
        RoomAdminError::RoomNotFound => "No room uses that address.",
        RoomAdminError::NoRoute => crate::room_admin::NO_ROUTE,
        RoomAdminError::UnsupportedContent => crate::room_admin::UNSUPPORTED_CONTENT,
        RoomAdminError::TooManyChanges => {
            "Moving it needs too many order keys at once. Give nearby entries order keys first."
        }
        RoomAdminError::InvalidName => "Enter a room name of up to 255 bytes, on one line.",
        // Invitations to the Space.
        RoomAdminError::AlreadyJoined => "They are already a member of this Space.",
        RoomAdminError::AlreadyInvited => "They already have a pending invitation.",
        RoomAdminError::TargetBanned => "They are banned from this Space. Unban them first.",
        RoomAdminError::InvalidUserId => BAD_USER_ID,
        RoomAdminError::CreatorProtected
        | RoomAdminError::SelfTarget
        | RoomAdminError::InvalidPowerLevel
        | RoomAdminError::InvalidReason => "This change cannot be made here.",
    };
    text.to_owned()
}

/// Failures that may follow a sent write re-read the hierarchy instead of
/// resending; input mistakes and closed views do not.
pub(crate) fn reread_after(error: RoomAdminError) -> bool {
    !matches!(
        error,
        RoomAdminError::Cancelled
            | RoomAdminError::AccountChanged
            | RoomAdminError::InvalidRoom
            | RoomAdminError::RoomNotFound
            | RoomAdminError::NoRoute
            | RoomAdminError::InvalidName
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SPACE: &str = "!space:example.org";

    fn link(parent: &str, room: &str, ts: u64) -> SpaceChildLink {
        let content = json!({"via": ["example.org"]});
        SpaceChildLink::from_content(parent, room, ts, content).unwrap()
    }
    fn node(room: &str, joined: bool, children: Vec<SpaceChildLink>) -> SpaceNode {
        SpaceNode {
            room_id: room.to_owned(),
            name: None,
            topic: None,
            canonical_alias: None,
            is_space: !children.is_empty(),
            joined_members: 1,
            access: NodeAccess::Public,
            world_readable: false,
            membership: joined.then_some(RoomMembership::Joined),
            children,
            children_truncated: false,
        }
    }

    #[test]
    fn cycles_and_shared_children_are_listed_but_expanded_once() {
        let a = "!a:example.org";
        let b = "!b:example.org";
        let nodes = [
            node(SPACE, true, vec![link(SPACE, a, 1), link(SPACE, b, 2)]),
            node(a, true, vec![link(a, b, 1), link(a, SPACE, 2)]),
            node(b, true, vec![link(b, a, 1)]),
        ];
        let rows = rows(SPACE, &nodes);
        let listed = rows
            .iter()
            .map(|row| (row.link.room_id.as_str(), row.depth, row.repeated))
            .collect::<Vec<_>>();
        let expected = [
            (a, 1, false),
            (b, 2, false),
            (a, 3, true),
            (SPACE, 2, true),
            (b, 1, true),
        ];
        assert_eq!(listed, expected);
    }

    #[test]
    fn depth_is_capped_and_filters_split_joined_from_not_joined() {
        let a = "!a:example.org";
        let b = "!b:example.org";
        let c = "!c:example.org";
        let d = "!d:example.org";
        let nodes = [
            node(SPACE, true, vec![link(SPACE, a, 1)]),
            node(a, true, vec![link(a, b, 1)]),
            node(b, false, vec![link(b, c, 1)]),
            node(c, true, vec![link(c, d, 1)]),
        ];
        let rows = rows(SPACE, &nodes);
        let depths = rows.iter().map(|row| row.depth).collect::<Vec<_>>();
        assert_eq!(depths, [1, 2, 3]);
        let (joined, total) = visible(&rows, &nodes, "", Filter::Joined);
        assert_eq!((joined.len(), total), (2, 2));
        let (others, _) = visible(&rows, &nodes, "", Filter::NotJoined);
        assert_eq!(others[0].link.room_id, b);
        let (found, _) = visible(&rows, &nodes, "!C:EXAMPLE", Filter::All);
        assert_eq!(found.len(), 1);
        assert_eq!(visible(&rows, &nodes, "", Filter::Suggested).1, 0);
        let mut suggested = nodes.clone();
        suggested[1].children[0].suggested = true;
        let rows = super::rows(SPACE, &suggested);
        let (shown, total) = visible(&rows, &suggested, "", Filter::Suggested);
        assert_eq!((shown[0].link.room_id.as_str(), total), (b, 1));
        assert_eq!(Filter::from_index(4), Filter::Suggested);
    }

    #[test]
    fn creating_a_room_is_reviewed_and_a_failed_listing_is_reported() {
        let change = Change::Create {
            name: "Planning".to_owned(),
            is_space: false,
            members_only: true,
            suggested: false,
        };
        let intent = Intent {
            change,
            name: "Planning".to_owned(),
            space: "Crew".to_owned(),
        };
        let review = confirmation(&intent);
        assert!(!review.destructive && review.body.contains("Members of Crew can join it"));
        assert!(review.body.contains("a new encrypted room"));
        assert!(intent.still_matches(&[]));
        // The room does not exist yet, so no row is marked pending.
        assert_eq!(intent.room_id(), "");
        let listed = CreatedChild {
            room_id: "!new:example.org".to_owned(),
            linked: true,
            link_error: None,
        };
        let (notice, error) = created_text(&intent, &listed);
        assert_eq!(notice, "Created Planning and listed it in Crew.");
        assert!(error.is_empty());
        let unlisted = CreatedChild {
            linked: false,
            link_error: Some(RoomAdminError::Forbidden),
            ..listed
        };
        let (notice, error) = created_text(&intent, &unlisted);
        assert!(notice.contains("!new:example.org"));
        assert!(error.starts_with("Listing it in Crew failed:"));
        assert!(!reread_after(RoomAdminError::InvalidName));
    }

    #[test]
    fn unlink_is_destructive_and_only_joined_sub_spaces_open_here() {
        let a = "!a:example.org";
        let nodes = [
            node(SPACE, true, vec![link(SPACE, a, 1)]),
            node(a, true, vec![link(a, SPACE, 1)]),
        ];
        let rows = rows(SPACE, &nodes);
        let opened = openable_space(&rows[0], &nodes, SPACE);
        assert_eq!(opened.map(|(id, _)| id).as_deref(), Some(a));
        assert!(openable_space(&rows[1], &nodes, SPACE).is_none());
        let link = rows[0].link.clone();
        let intent = Intent {
            change: Change::Unlink { link },
            name: "Room".to_owned(),
            space: "Space".to_owned(),
        };
        let confirmation = confirmation(&intent);
        assert!(confirmation.destructive);
        assert!(confirmation.body.contains("Nobody leaves"));
    }

    #[test]
    fn inviting_to_the_space_is_reviewed_and_membership_refusals_are_named() {
        let kim = parse_user_id(" @kim:example.org ").unwrap();
        assert_eq!(kim, "@kim:example.org");
        for invalid in ["kim", "@kim", "@ki m:example.org", ""] {
            assert_eq!(parse_user_id(invalid), Err(BAD_USER_ID.to_owned()));
        }
        let intent = Intent {
            change: Change::Invite {
                user_id: "@kim:example.org".to_owned(),
            },
            name: "@kim:example.org".to_owned(),
            space: "Crew".to_owned(),
        };
        assert_eq!(intent.room_id(), "");
        let review = confirmation(&intent);
        assert!(!review.destructive && review.title == "Invite @kim:example.org to Crew?");
        assert!(review.body.contains("Each room keeps its own rules"));
        let invited = SpaceWriteOutcome {
            accepted: 1,
            requested: 1,
            confirmed: true,
            stopped: None,
        };
        let text = outcome_text(&intent, invited);
        assert!(text.starts_with("Invited @kim:example.org to Crew."));
        let already = error_text(RoomAdminError::AlreadyJoined);
        assert_eq!(already, "They are already a member of this Space.");
    }

    #[test]
    fn removing_several_entries_is_destructive_and_fenced_by_each_link() {
        let (a, b) = ("!a:example.org", "!b:example.org");
        let links = vec![link(SPACE, a, 1), link(SPACE, b, 2)];
        let intent = Intent {
            change: Change::UnlinkMany {
                links: links.clone(),
            },
            name: entries_text(2),
            space: "Crew".to_owned(),
        };
        assert_eq!(entries_text(1), "1 entry");
        assert_eq!(intent.room_id(), "");
        assert!(intent.still_matches(&links));
        // Any reviewed link that changed withdraws the whole review.
        assert!(!intent.still_matches(&links[..1]));
        let review = confirmation(&intent);
        assert!(review.destructive && review.title == "Remove 2 entries from Crew?");
        assert!(review.body.contains("Nobody leaves"));
        let pending = pending_text(&intent);
        assert_eq!(pending, "Removing 2 entries… Waiting for the homeserver.");
        let partial = SpaceWriteOutcome {
            accepted: 1,
            requested: 2,
            confirmed: false,
            stopped: Some(RoomAdminError::TargetChanged),
        };
        let text = outcome_text(&intent, partial);
        assert!(text.starts_with("1 of 2 entries were removed from Crew, then:"));
        let nodes = [node(a, true, Vec::new())];
        assert_eq!(entry_detail(&links[0], Some(&nodes[0])), "Room · Joined");
        assert_eq!(removable(&links).len(), 2);
    }

    #[test]
    fn adding_several_rooms_is_reviewed_and_a_partial_result_names_the_failure() {
        let intent = Intent {
            change: Change::LinkMany {
                targets: Vec::new(),
                suggested: true,
            },
            name: rooms_text(3),
            space: "Crew".to_owned(),
        };
        assert_eq!(rooms_text(1), "1 room");
        // New entries are not rows yet, and links are refused if already live.
        assert_eq!(intent.room_id(), "");
        assert!(intent.still_matches(&[]));
        let review = confirmation(&intent);
        assert!(!review.destructive && review.title == "Add 3 rooms to Crew?");
        assert!(review.body.contains("first failure stops the rest"));
        assert!(review.body.contains("suggested to members"));
        let pending = pending_text(&intent);
        assert_eq!(pending, "Adding 3 rooms… Waiting for the homeserver.");
        let all = SpaceWriteOutcome {
            accepted: 3,
            requested: 3,
            confirmed: true,
            stopped: None,
        };
        assert_eq!(
            outcome_text(&intent, all),
            "Added 3 rooms to Crew. The homeserver confirmed each link."
        );
        let partial = SpaceWriteOutcome {
            accepted: 1,
            confirmed: false,
            stopped: Some(RoomAdminError::Forbidden),
            ..all
        };
        let text = outcome_text(&intent, partial);
        assert!(text.starts_with("1 of 3 rooms were added to Crew, then:"));
        assert!(too_many_picked().contains(&MAX_LINKS_AT_ONCE.to_string()));
    }

    #[test]
    fn an_upgraded_room_is_replaced_in_place_and_a_partial_result_is_reported() {
        let (old, new) = ("!old:example.org", "!new:example.org");
        let mut replaced = link(SPACE, old, 1);
        replaced.replaced_by = Some(new.to_owned());
        let nodes = [
            node(SPACE, true, vec![replaced.clone()]),
            node(old, true, Vec::new()),
        ];
        let rows = rows(SPACE, &nodes);
        assert!(row_detail(&rows[0], &nodes).ends_with("Replaced by a newer room"));
        let detail = selected_detail(&rows[0], &nodes, true);
        assert!(detail.ends_with("This room was upgraded. Its replacement is !new:example.org."));
        let change = Change::Replace {
            link: replaced.clone(),
            replacement: new.to_owned(),
        };
        let intent = Intent {
            change,
            name: "Old".to_owned(),
            space: "Crew".to_owned(),
        };
        assert_eq!(intent.room_id(), old);
        assert!(intent.still_matches(std::slice::from_ref(&replaced)));
        assert!(!intent.still_matches(&[link(SPACE, old, 2)]));
        let review = confirmation(&intent);
        assert!(!review.destructive && review.body.contains("Nobody joins or leaves"));
        assert!(review.body.contains("upgraded to !new:example.org"));
        assert_eq!(
            pending_text(&intent),
            "Replacing Old… Waiting for the homeserver."
        );
        let both = SpaceWriteOutcome {
            accepted: 2,
            requested: 2,
            confirmed: true,
            stopped: None,
        };
        assert_eq!(
            outcome_text(&intent, both),
            "Crew now lists the new room for Old. The homeserver confirmed it."
        );
        // The new room is listed first, so a later failure leaves both listed.
        let half = SpaceWriteOutcome {
            accepted: 1,
            stopped: Some(RoomAdminError::Forbidden),
            ..both
        };
        assert!(outcome_text(&intent, half).contains("removing the old link failed"));
    }
}
