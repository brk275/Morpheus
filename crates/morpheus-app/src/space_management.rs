//! Pure rules for native Space management: the bounded hierarchy tree,
//! search, effect descriptions, deliberate confirmation and fixed messages.
//! The UI adapter in `ui.rs` holds the account selection, the exact Space and
//! a per-panel generation across every asynchronous result.

pub(crate) mod ui;

pub(crate) use ui::clear_presentation;

use std::collections::{HashMap, HashSet};

use morpheus_matrix::{
    HIERARCHY_MAX_DEPTH, NodeAccess, RoomAdminError, RoomMembership, RouteSource, SpaceChildLink,
    SpaceNode, SpaceTarget, SpaceWriteOutcome, reorder_plan, valid_order,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Filter {
    All,
    Joined,
    NotJoined,
    Spaces,
}

impl Filter {
    pub(crate) fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Joined,
            2 => Self::NotJoined,
            3 => Self::Spaces,
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
}

pub(crate) fn reading_text(reading: Reading) -> &'static str {
    match reading {
        Reading::First => "Loading the Space from your homeserver…",
        Reading::Refresh => "Refreshing the Space…",
        Reading::More => "Loading more of the Space…",
        Reading::Find => "Finding the room through your homeserver…",
    }
}

pub(crate) fn stopped_text(reading: Reading) -> &'static str {
    match reading {
        Reading::First => "Stopped loading the Space. Refresh to try again.",
        Reading::Refresh => "Stopped refreshing. The list may be out of date.",
        Reading::More => "Stopped loading more. The entries already shown are kept.",
        Reading::Find => "Stopped finding the room. Nothing was added.",
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
        if !admits(filter, node) || !matches_query(&row.link.room_id, node, &needle) {
            continue;
        }
        total += 1;
        if shown.len() < MAX_VISIBLE_ROWS {
            shown.push(row);
        }
    }
    (shown, total)
}

fn admits(filter: Filter, node: Option<&SpaceNode>) -> bool {
    let joined = node.is_some_and(|node| node.membership == Some(RoomMembership::Joined));
    match filter {
        Filter::All => true,
        Filter::Joined => joined,
        Filter::NotJoined => !joined,
        Filter::Spaces => node.is_some_and(|node| node.is_space),
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
    format!("{name}\n{kind} · {membership} · {room}\n{}", route_text(target))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    Link {
        target: SpaceTarget,
        suggested: bool,
        order: Option<String>,
    },
    Update {
        link: SpaceChildLink,
        suggested: bool,
        order: Option<String>,
    },
    Unlink { link: SpaceChildLink },
    Reorder {
        plan: Vec<(SpaceChildLink, String)>,
        up: bool,
    },
    Join {
        room_id: String,
        via: Vec<String>,
        is_space: bool,
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
            Change::Update { link, .. } | Change::Unlink { link } => link.room_id.as_str(),
            Change::Reorder { plan, .. } => {
                plan.first().map_or("", |(link, _)| link.room_id.as_str())
            }
            Change::Join { room_id, .. } => room_id.as_str(),
        }
    }
    /// Every reviewed link is still listed unchanged by a fresher load.
    pub(crate) fn still_matches(&self, direct: &[SpaceChildLink]) -> bool {
        let listed = |link: &SpaceChildLink| direct.iter().any(|item| item == link);
        match &self.change {
            Change::Update { link, .. } | Change::Unlink { link } => listed(link),
            Change::Reorder { plan, .. } => plan.iter().all(|(link, _)| listed(link)),
            Change::Link { .. } | Change::Join { .. } => true,
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
        Change::Join { is_space, .. } => {
            let kind = if *is_space { "Space" } else { "room" };
            Confirmation {
                title: format!("Join {name}?"),
                body: format!("This joins the {kind} {name}. {JOIN_BODY}"),
                action: "Join",
                destructive: false,
            }
        }
    }
}

pub(crate) fn pending_text(intent: &Intent) -> String {
    let verb = match intent.change {
        Change::Link { .. } => "Adding",
        Change::Update { .. } => "Updating",
        Change::Unlink { .. } => "Removing",
        Change::Reorder { .. } => "Moving",
        Change::Join { .. } => "Joining",
    };
    format!("{verb} {}… Waiting for the homeserver.", intent.name)
}

/// Report only what the homeserver accepted and what reading it back showed.
pub(crate) fn outcome_text(intent: &Intent, outcome: SpaceWriteOutcome) -> String {
    let name = &intent.name;
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
        Change::Update { .. } => format!("Updated {name}. The homeserver confirmed it."),
        Change::Unlink { .. } => format!("Removed the link to {name}; the room is unchanged."),
        Change::Reorder { .. } => format!("Moved {name}. The homeserver confirmed the order."),
        Change::Join { .. } => format!("Joined {name}."),
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
        RoomAdminError::CreatorProtected
        | RoomAdminError::SelfTarget
        | RoomAdminError::AlreadyJoined
        | RoomAdminError::AlreadyInvited
        | RoomAdminError::TargetBanned
        | RoomAdminError::InvalidUserId
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
}
