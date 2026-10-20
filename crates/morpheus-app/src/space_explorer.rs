//! The read-only Space explorer: browse a Space's rooms and nested Spaces a
//! level at a time, before joining any of them.
//!
//! Only the level on screen is read, one bounded page per open, Browse or
//! Load more, through the active account's pinned client
//! (`AccountManager::space_level_for`). Nothing is fetched eagerly or for a
//! Space the user did not open, nothing is joined without an explicit Join,
//! and no media is read: rows show a monogram. Every page and join is bound
//! to the exact `AccountSelection` and to an operation number that each
//! open, navigation and clear advances, so a result for a departed account,
//! level or explorer (A-B-A included) is dropped. Detaching, switching or
//! closing clears every private name and row synchronously.
//!
//! Joins go through `LiveUi::join_target_for` (the one bounded join for the
//! exact selection, shared with the directory) and Open through the
//! existing `select-space` / `open-room` routing. Nothing here edits a Space
//! or its links. Closing or clearing the explorer withdraws its join: before
//! the hand-off nothing is sent, after it nothing is opened.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use morpheus_matrix::{
    AccountSelection, MatrixCoreError, MatrixLinkTarget, RoomMembership, SpaceChildInfo,
    SpaceJoinRule, SpaceLevelEntry, SpaceLevelError, SpaceLevelPage,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::runtime::Handle;

use crate::{
    AppWindow, SpaceExplorer, SpaceExplorerCrumb, SpaceExplorerRow,
    live::{JoinAsked, LiveUi},
};

#[cfg(test)]
mod tests;

/// Spaces deep one breadcrumb trail goes.
pub(crate) const MAX_TRAIL: usize = 8;
/// Pages one level follows before it stops offering more.
pub(crate) const MAX_LEVEL_PAGES: usize = 10;
/// Rooms one level lists.
pub(crate) const MAX_LEVEL_ROWS: usize = 500;
/// Rooms the whole trail keeps; going back frees a level's rows.
pub(crate) const MAX_LOADED_ROWS: usize = 2000;
const MAX_QUERY_CHARS: usize = 64;
const MAX_LABEL_CHARS: usize = 120;
const PAGE_TIMEOUT: Duration = Duration::from_secs(30);
const JOIN_TIMEOUT: Duration = Duration::from_secs(60);

/// What this account may do with one listed room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Joined,
    Invited,
    Public,
    Restricted,
    Knock,
    InviteOnly,
    Banned,
    Unsupported,
    Unavailable,
    UnknownRule,
}

impl Status {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Joined => "Joined",
            Self::Invited => "Invited",
            Self::Public => "Anyone can join",
            Self::Restricted => "Members of certain Spaces or rooms can join",
            Self::Knock => "Asks to join; not available here",
            Self::InviteOnly => "Invite only",
            Self::Banned => "Banned",
            Self::Unsupported => "Room type not supported",
            Self::Unavailable => "Not previewable from your homeserver",
            Self::UnknownRule => "Unknown join rule",
        }
    }
    pub(crate) fn joinable(self) -> bool {
        matches!(self, Self::Invited | Self::Public | Self::Restricted)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) room_id: String,
    /// Bounded plain text: the name, else the address, else the room ID.
    pub(crate) name: String,
    pub(crate) alias: String,
    pub(crate) topic: String,
    pub(crate) members: u64,
    pub(crate) is_space: bool,
    pub(crate) unsupported: bool,
    pub(crate) join_rule: SpaceJoinRule,
    pub(crate) membership: Option<RoomMembership>,
    pub(crate) suggested: bool,
    /// Validated route servers from the parent's link, for a join.
    pub(crate) via: Vec<String>,
    /// Links a Space row declares; `None` for a room.
    pub(crate) links: Option<usize>,
    /// Linked by the Space but not described by the homeserver.
    pub(crate) missing: bool,
}

fn status(
    membership: Option<RoomMembership>,
    missing: bool,
    unsupported: bool,
    join_rule: SpaceJoinRule,
) -> Status {
    match membership {
        Some(RoomMembership::Joined) => return Status::Joined,
        Some(RoomMembership::Invited) => return Status::Invited,
        Some(RoomMembership::Banned) => return Status::Banned,
        _ => {}
    }
    if missing {
        return Status::Unavailable;
    }
    if unsupported {
        return Status::Unsupported;
    }
    match join_rule {
        SpaceJoinRule::Public => Status::Public,
        SpaceJoinRule::Restricted | SpaceJoinRule::KnockRestricted => Status::Restricted,
        SpaceJoinRule::Knock => Status::Knock,
        SpaceJoinRule::Invite | SpaceJoinRule::Private => Status::InviteOnly,
        SpaceJoinRule::Other => Status::UnknownRule,
    }
}

impl Row {
    pub(crate) fn status(&self) -> Status {
        status(
            self.membership,
            self.missing,
            self.unsupported,
            self.join_rule,
        )
    }
    fn matches(&self, query: &str) -> bool {
        [&self.name, &self.alias, &self.topic, &self.room_id]
            .iter()
            .any(|field| field.to_lowercase().contains(query))
    }
}

/// One Space on the breadcrumb trail and the rows loaded for it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Level {
    pub(crate) space_id: String,
    pub(crate) name: String,
    /// Route servers this Space was reached by, for joining it.
    pub(crate) via: Vec<String>,
    /// The Space's own links, once its entry arrived.
    links: Option<Vec<SpaceChildInfo>>,
    pub(crate) links_truncated: bool,
    pub(crate) membership: Option<RoomMembership>,
    /// The Space's own join rule and room type, once its entry arrived.
    rule: Option<(SpaceJoinRule, bool)>,
    linked: HashMap<String, RoomMembership>,
    pub(crate) rows: Vec<Row>,
    seen: HashSet<String>,
    next_batch: Option<String>,
    pub(crate) pages: usize,
    pub(crate) complete: bool,
    /// Rows left out at a level or trail bound.
    pub(crate) capped: bool,
    pub(crate) error: Option<SpaceLevelError>,
}

impl Level {
    fn new(space_id: &str, name: &str, via: Vec<String>) -> Self {
        Self {
            space_id: space_id.to_owned(),
            name: label(name, space_id),
            via,
            ..Self::default()
        }
    }
    /// Whether another page may be asked for now.
    fn wants_page(&self, loaded: usize) -> bool {
        self.error.is_none()
            && !self.complete
            && !self.capped
            && self.pages < MAX_LEVEL_PAGES
            && self.rows.len() < MAX_LEVEL_ROWS
            && loaded < MAX_LOADED_ROWS
    }
    fn link(&self, room_id: &str) -> Option<&SpaceChildInfo> {
        self.links
            .as_ref()?
            .iter()
            .find(|link| link.room_id == room_id)
    }
}

/// A page request for the level on top of the trail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) operation: u64,
    pub(crate) space_id: String,
    pub(crate) from: Option<String>,
}

/// The explorer's state for one account selection. Pure: no Slint, client
/// or task; the UI glue below owns those.
#[derive(Debug)]
pub(crate) struct Explorer {
    operation: u64,
    pub(crate) trail: Vec<Level>,
    query: String,
    pending: Option<Request>,
    /// The room being joined, and the join's ticket.
    joining: Option<(String, u64)>,
    pub(crate) notice: String,
}

impl Explorer {
    pub(crate) fn new(operation: u64, space_id: &str, name: &str, via: Vec<String>) -> Self {
        Self {
            operation,
            trail: vec![Level::new(space_id, name, via)],
            query: String::new(),
            pending: None,
            joining: None,
            notice: String::new(),
        }
    }
    pub(crate) fn top(&self) -> &Level {
        self.trail
            .last()
            .expect("the trail always holds its first Space")
    }
    fn top_mut(&mut self) -> &mut Level {
        self.trail
            .last_mut()
            .expect("the trail always holds its first Space")
    }
    fn loaded(&self) -> usize {
        self.trail.iter().map(|level| level.rows.len()).sum()
    }
    pub(crate) fn loading(&self) -> bool {
        self.pending.is_some()
    }
    pub(crate) fn waits_for(&self, request: &Request) -> bool {
        self.pending.as_ref() == Some(request)
    }
    pub(crate) fn can_load_more(&self) -> bool {
        self.pending.is_none() && self.top().pages > 0 && self.top().wants_page(self.loaded())
    }
    /// A navigation retires every request in flight for another level.
    fn navigated(&mut self, operation: u64) {
        self.operation = operation;
        self.pending = None;
        self.query.clear();
        self.notice.clear();
    }

    /// The next page of the level on top, if one may be asked for now.
    pub(crate) fn next_request(&mut self) -> Option<Request> {
        let loaded = self.loaded();
        let level = self.top();
        if self.pending.is_some() || !level.wants_page(loaded) {
            return None;
        }
        let request = Request {
            operation: self.operation,
            space_id: level.space_id.clone(),
            from: level.next_batch.clone(),
        };
        if level.pages > 0 && request.from.is_none() {
            return None;
        }
        self.pending = Some(request.clone());
        Some(request)
    }

    /// Retry the level on top after an error.
    pub(crate) fn retry(&mut self) -> Option<Request> {
        self.top_mut().error = None;
        self.next_request()
    }

    /// Apply the result of `request`. Returns false, changing nothing, for
    /// a request this explorer no longer waits for.
    pub(crate) fn apply(
        &mut self,
        request: &Request,
        result: Result<SpaceLevelPage, SpaceLevelError>,
    ) -> bool {
        if self.pending.as_ref() != Some(request) || self.top().space_id != request.space_id {
            return false;
        }
        self.pending = None;
        let loaded = self.loaded();
        let level = self.top_mut();
        let page = match result {
            Ok(page) if page.space_id == level.space_id => page,
            Ok(_) => {
                level.error = Some(SpaceLevelError::Unavailable);
                return true;
            }
            Err(error) => {
                level.error = Some(error);
                return true;
            }
        };
        level.pages += 1;
        level.capped |= page.truncated;
        if let Some(space) = page.space {
            level.membership = space.membership;
            if let Some(name) = space.node.name.as_deref() {
                level.name = label(name, &level.space_id);
            }
            level.links_truncated = space.links_truncated;
            level.rule = Some((space.node.join_rule, space.node.unsupported_type));
            level.links = Some(space.node.children);
        }
        level
            .linked
            .extend(page.linked_memberships.into_iter().take(MAX_LEVEL_ROWS));
        let mut room = loaded;
        for entry in page.entries {
            if level.rows.len() >= MAX_LEVEL_ROWS || room >= MAX_LOADED_ROWS {
                level.capped = true;
                break;
            }
            let id = entry.node.room_id.clone();
            let link = level.link(&id).cloned();
            // Only the Space's own children belong on its level; a room the
            // server adds that the Space does not link is left out.
            let linked = level.links.is_none() || level.links_truncated || link.is_some();
            if !linked || !level.seen.insert(id) {
                continue;
            }
            level.rows.push(row(entry, link.as_ref()));
            room += 1;
        }
        // An unusable/repeated cursor or a bounded partial page must not turn
        // unseen children into a claim that the server cannot preview them.
        let repeated = page.next_batch.is_some() && page.next_batch == request.from;
        level.capped |= repeated || (page.next_batch.is_some() && level.pages >= MAX_LEVEL_PAGES);
        level.next_batch = if repeated { None } else { page.next_batch };
        level.complete = level.next_batch.is_none() && !level.capped;
        if level.complete {
            level.add_missing(room);
        }
        true
    }

    /// Show a child Space as the next level, or go back to it when it is
    /// already on the trail: a Space that links an ancestor is a cycle and
    /// is never read again.
    pub(crate) fn browse(&mut self, room_id: &str, operation: u64) -> bool {
        if let Some(index) = self
            .trail
            .iter()
            .position(|level| level.space_id == room_id)
        {
            return self.crumb(index, operation);
        }
        if self.trail.len() >= MAX_TRAIL {
            self.notice = format!("Spaces nested deeper than {MAX_TRAIL} levels are not shown.");
            return false;
        }
        let Some(row) = self.top().rows.iter().find(|row| row.room_id == room_id) else {
            return false;
        };
        if !row.is_space || row.missing {
            return false;
        }
        let level = Level::new(&row.room_id, &row.name, row.via.clone());
        self.navigated(operation);
        self.trail.push(level);
        true
    }

    /// Go back to the Space at `index` on the trail, dropping deeper levels.
    pub(crate) fn crumb(&mut self, index: usize, operation: u64) -> bool {
        if index + 1 >= self.trail.len() {
            return false;
        }
        self.navigated(operation);
        self.trail.truncate(index + 1);
        true
    }

    /// Go back one level. False at the first Space: the caller closes.
    pub(crate) fn back(&mut self, operation: u64) -> bool {
        let depth = self.trail.len();
        depth > 1 && self.crumb(depth - 2, operation)
    }

    pub(crate) fn set_query(&mut self, query: &str) {
        self.query = query
            .chars()
            .filter(|ch| !ch.is_control())
            .take(MAX_QUERY_CHARS)
            .collect::<String>()
            .trim()
            .to_lowercase();
    }

    /// The level's rows the search matches, in their listed order.
    pub(crate) fn visible(&self) -> impl Iterator<Item = &Row> {
        let query = self.query.as_str();
        self.top()
            .rows
            .iter()
            .filter(move |row| query.is_empty() || row.matches(query))
    }

    pub(crate) fn in_trail(&self, room_id: &str) -> bool {
        self.trail.iter().any(|level| level.space_id == room_id)
    }

    /// The row or the shown Space with `room_id`, as a join target.
    pub(crate) fn join_target(&self, room_id: &str) -> Option<JoinTarget> {
        let level = self.top();
        if level.space_id == room_id {
            // A Space the homeserver could not describe may still admit a
            // deliberate join; the server decides.
            let status = level
                .rule
                .map(|(rule, unsupported)| status(level.membership, false, unsupported, rule));
            let joinable = match status {
                Some(status) => status.joinable(),
                None => !matches!(
                    level.membership,
                    Some(RoomMembership::Joined | RoomMembership::Banned)
                ),
            };
            return joinable.then(|| JoinTarget {
                room_id: level.space_id.clone(),
                via: level.via.clone(),
                restricted: status == Some(Status::Restricted),
            });
        }
        let row = level.rows.iter().find(|row| row.room_id == room_id)?;
        let status = row.status();
        status.joinable().then(|| JoinTarget {
            room_id: row.room_id.clone(),
            via: row.via.clone(),
            restricted: status == Status::Restricted,
        })
    }

    pub(crate) fn begin_join(&mut self, room_id: &str, ticket: u64) -> bool {
        if self.joining.is_some() {
            return false;
        }
        self.joining = Some((room_id.to_owned(), ticket));
        self.notice.clear();
        true
    }
    pub(crate) fn joining(&self) -> Option<&str> {
        self.joining.as_ref().map(|(room_id, _)| room_id.as_str())
    }

    /// Finish the join with `ticket`. Every level that lists the room shows
    /// it as joined: one room may be in several Spaces.
    pub(crate) fn finish_join(&mut self, ticket: u64, joined: bool, failure: &str) -> bool {
        let Some((room_id, current)) = self.joining.take() else {
            return false;
        };
        if current != ticket {
            self.joining = Some((room_id, current));
            return false;
        }
        if !joined {
            self.notice = failure.to_owned();
            return true;
        }
        for level in &mut self.trail {
            if level.space_id == room_id {
                level.membership = Some(RoomMembership::Joined);
                level.error = None;
            }
            level.linked.insert(room_id.clone(), RoomMembership::Joined);
            for row in level.rows.iter_mut().filter(|row| row.room_id == room_id) {
                row.membership = Some(RoomMembership::Joined);
            }
        }
        self.notice = "Joined.".into();
        true
    }

    /// The joined row or shown Space with `room_id`, to open.
    pub(crate) fn open_target(&self, room_id: &str) -> Option<(String, String, bool)> {
        let level = self.top();
        if level.space_id == room_id {
            return (level.membership == Some(RoomMembership::Joined))
                .then(|| (level.space_id.clone(), level.name.clone(), true));
        }
        let row = level.rows.iter().find(|row| row.room_id == room_id)?;
        (row.status() == Status::Joined && !row.unsupported)
            .then(|| (row.room_id.clone(), row.name.clone(), row.is_space))
    }
}

impl Level {
    /// After the last page, list the links the homeserver did not describe.
    fn add_missing(&mut self, mut loaded: usize) {
        let Some(links) = &self.links else {
            return;
        };
        for link in links {
            if self.rows.len() >= MAX_LEVEL_ROWS || loaded >= MAX_LOADED_ROWS {
                self.capped = true;
                return;
            }
            if !self.seen.insert(link.room_id.clone()) {
                continue;
            }
            self.rows.push(Row {
                room_id: link.room_id.clone(),
                name: link.room_id.clone(),
                alias: String::new(),
                topic: String::new(),
                members: 0,
                is_space: false,
                unsupported: false,
                join_rule: SpaceJoinRule::Other,
                membership: self.linked.get(&link.room_id).copied(),
                suggested: link.suggested,
                via: link.via_servers.clone(),
                links: None,
                missing: true,
            });
            loaded += 1;
        }
    }
}

fn row(entry: SpaceLevelEntry, link: Option<&SpaceChildInfo>) -> Row {
    let node = entry.node;
    let alias = node.canonical_alias.unwrap_or_default();
    let name = node.name.unwrap_or_else(|| {
        if alias.is_empty() {
            node.room_id.clone()
        } else {
            alias.clone()
        }
    });
    Row {
        name: label(&name, &node.room_id),
        alias,
        topic: node.topic.unwrap_or_default(),
        members: node.joined_members,
        is_space: node.is_space,
        unsupported: node.unsupported_type,
        join_rule: node.join_rule,
        membership: entry.membership,
        suggested: link.is_some_and(|link| link.suggested),
        via: link
            .map(|link| link.via_servers.clone())
            .unwrap_or_default(),
        links: node.is_space.then_some(node.children.len()),
        missing: false,
        room_id: node.room_id,
    }
}

/// A bounded single-line label, or the room ID when nothing printable is left.
fn label(text: &str, fallback: &str) -> String {
    let text = crate::directory_plain_text(text, MAX_LABEL_CHARS);
    if text.is_empty() {
        crate::directory_plain_text(fallback, MAX_LABEL_CHARS)
    } else {
        text
    }
}

/// What a join needs: whether it is a Space, and its name, come from sync
/// once it is joined (LiveUi::join_target_for), never from the preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JoinTarget {
    pub(crate) room_id: String,
    pub(crate) via: Vec<String>,
    pub(crate) restricted: bool,
}

impl JoinTarget {
    fn link(&self) -> MatrixLinkTarget {
        let mut via_servers = self.via.clone();
        if via_servers.is_empty()
            && let Some(server) = crate::matrix_id_server(&self.room_id)
        {
            via_servers.push(server.to_owned());
        }
        MatrixLinkTarget::Room {
            room_or_alias_id: self.room_id.clone(),
            event_id: None,
            via_servers,
        }
    }
    /// A fixed sentence; the homeserver's reason is never shown.
    fn failure(&self, error: &MatrixCoreError) -> &'static str {
        use morpheus_matrix::JoinError;
        match error {
            MatrixCoreError::SelectedAccountChanged
            | MatrixCoreError::Join(JoinError::AccountChanged) => {
                "The account changed before joining. Nothing was sent."
            }
            MatrixCoreError::Join(JoinError::Cancelled) => {
                "The join was cancelled before it was sent. Nothing was sent."
            }
            MatrixCoreError::Join(JoinError::Blocked) => {
                "Finish the current security step first. Nothing was sent."
            }
            MatrixCoreError::Join(JoinError::Unavailable) => {
                "This account cannot join rooms right now. Nothing was sent."
            }
            MatrixCoreError::Join(JoinError::Pending) => {
                "A join of this room is still waiting for its reply. Nothing more was sent."
            }
            MatrixCoreError::Join(JoinError::EarlierUnknown) => {
                "An earlier join of this room may or may not have happened. Further joins of it are paused in this session. Check your room list. Nothing was sent."
            }
            MatrixCoreError::Join(JoinError::UnresolvedLimit) => {
                "Other joins still need checking. Further joins are paused in this session. Nothing was sent."
            }
            // Never taken as this room: nothing opens.
            MatrixCoreError::Join(JoinError::UnexpectedRoom) => {
                "The homeserver answered for a different room, so nothing was opened. Check your room list before joining again."
            }
            // Sent, and no definite reply: never "could not join".
            MatrixCoreError::Join(error) if error.may_have_joined() => {
                "No reply arrived, so you may or may not have joined. Check your room list before joining again."
            }
            _ if self.restricted => {
                "Could not join. Only members of certain Spaces or rooms can join this one."
            }
            _ => "Could not join. The room may be invite only, full, or unreachable.",
        }
    }
}

struct Session {
    selection: AccountSelection,
    explorer: Explorer,
}

thread_local! {
    // Slint callbacks and every publication run on the UI thread only.
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    // Each open, navigation, join and clear takes a new number, so nothing
    // started for an earlier one is ever taken.
    static OPERATION: Cell<u64> = const { Cell::new(0) };
    static ROWS: Rc<VecModel<SpaceExplorerRow>> = Rc::new(VecModel::default());
    // The explorer's join in progress, with its operation number; closing
    // the explorer (or anything that clears it) withdraws it.
    static JOINING: RefCell<Option<(u64, JoinAsked)>> = const { RefCell::new(None) };
    static CRUMBS: Rc<VecModel<SpaceExplorerCrumb>> = Rc::new(VecModel::default());
}

fn next_operation() -> u64 {
    OPERATION.with(|operation| {
        let next = operation.get().wrapping_add(1);
        operation.set(next);
        next
    })
}

/// End the explorer and forget every name and row it showed, now.
pub(crate) fn clear(ui: &AppWindow) {
    next_operation();
    // Before its hand-off nothing is sent; after it nothing is opened, and
    // its outcome stays with LiveUi's join record.
    if let Some((_, asked)) = JOINING.with(|joining| joining.borrow_mut().take()) {
        asked.withdraw();
    }
    SESSION.with(|session| *session.borrow_mut() = None);
    ROWS.with(|rows| rows.set_vec(Vec::new()));
    CRUMBS.with(|crumbs| crumbs.set_vec(Vec::new()));
    let state = ui.global::<SpaceExplorer>();
    state.set_open(false);
    state.set_title("".into());
    state.set_summary("".into());
    state.set_space_status("".into());
    state.set_error("".into());
    state.set_notice("".into());
    state.set_query("".into());
    state.set_loading(false);
    state.set_can_load_more(false);
    state.set_can_join_space(false);
    state.set_can_open_space(false);
    state.set_space_id("".into());
    state.set_joining_id("".into());
}

/// Whether the explorer is open.
pub(crate) fn is_open(ui: &AppWindow) -> bool {
    ui.global::<SpaceExplorer>().get_open()
}

#[cfg(test)]
pub(crate) fn hold_join_for_test(ui: &AppWindow, asked: JoinAsked) {
    clear(ui);
    JOINING.with(|joining| *joining.borrow_mut() = Some((next_operation(), asked)));
    ui.global::<SpaceExplorer>().set_open(true);
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, handle: Handle) {
    let state = ui.global::<SpaceExplorer>();
    state.set_rows(ROWS.with(|rows| ModelRc::from(rows.clone())));
    state.set_crumbs(CRUMBS.with(|crumbs| ModelRc::from(crumbs.clone())));

    let weak = ui.as_weak();
    let (show_live, show_handle) = (live.clone(), handle.clone());
    state.on_show(move |space_id, name, route_server, alias| {
        if let Some(ui) = weak.upgrade() {
            let via = [route_server.as_str(), alias.as_str(), space_id.as_str()]
                .into_iter()
                .filter_map(|id| {
                    if id.starts_with(['!', '#']) {
                        crate::matrix_id_server(id)
                    } else {
                        (!id.is_empty()).then_some(id)
                    }
                })
                .fold(Vec::<String>::new(), |mut via, server| {
                    if !via.iter().any(|known| known == server) {
                        via.push(server.to_owned());
                    }
                    via
                });
            show(&ui, &show_live, &show_handle, &space_id, &name, via);
        }
    });

    let weak = ui.as_weak();
    let (browse_live, browse_handle) = (live.clone(), handle.clone());
    state.on_browse(move |room_id| {
        if let Some(ui) = weak.upgrade() {
            navigate(&ui, &browse_live, &browse_handle, |explorer, operation| {
                explorer.browse(&room_id, operation)
            });
        }
    });

    let weak = ui.as_weak();
    let (crumb_live, crumb_handle) = (live.clone(), handle.clone());
    state.on_crumb(move |index| {
        let (Some(ui), Ok(index)) = (weak.upgrade(), usize::try_from(index)) else {
            return;
        };
        navigate(&ui, &crumb_live, &crumb_handle, |explorer, operation| {
            explorer.crumb(index, operation)
        });
    });

    let weak = ui.as_weak();
    let (back_live, back_handle) = (live.clone(), handle.clone());
    state.on_back(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let first = SESSION.with(|session| {
            session
                .borrow()
                .as_ref()
                .is_none_or(|session| session.explorer.trail.len() < 2)
        });
        if first {
            clear(&ui);
        } else {
            navigate(&ui, &back_live, &back_handle, Explorer::back);
        }
    });

    let weak = ui.as_weak();
    let (more_live, more_handle) = (live.clone(), handle.clone());
    state.on_load_more(move || {
        if let Some(ui) = weak.upgrade() {
            step(&ui, &more_live, &more_handle, Explorer::next_request);
        }
    });

    let weak = ui.as_weak();
    let (retry_live, retry_handle) = (live.clone(), handle.clone());
    state.on_retry(move || {
        if let Some(ui) = weak.upgrade() {
            step(&ui, &retry_live, &retry_handle, Explorer::retry);
        }
    });

    let weak = ui.as_weak();
    state.on_filter(move |query| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let shown = SESSION.with(|session| {
            let mut session = session.borrow_mut();
            let session = session.as_mut()?;
            session.explorer.set_query(&query);
            Some(())
        });
        if shown.is_some() {
            publish(&ui);
        }
    });

    let weak = ui.as_weak();
    let join_live = live.clone();
    state.on_join(move |room_id| {
        if let Some(ui) = weak.upgrade() {
            join(&ui, &join_live, &handle, &room_id);
        }
    });

    let weak = ui.as_weak();
    state.on_open_room(move |room_id| {
        if let Some(ui) = weak.upgrade() {
            open(&ui, &live, &room_id);
        }
    });

    let weak = ui.as_weak();
    state.on_close(move || {
        if let Some(ui) = weak.upgrade() {
            clear(&ui);
        }
    });
}

/// The selection of the account on screen, if it may still read.
fn current_selection(ui: &AppWindow, live: &LiveUi) -> Option<AccountSelection> {
    if crate::room_addresses::is_open(ui)
        || !live.is_live.load(Ordering::Acquire)
        || !ui.get_discovery_context_allowed()
        || ui.get_room_actions_open()
        || live.security_gate.blocked()
    {
        return None;
    }
    let account_id = ui.get_active_account_local_id();
    if account_id.is_empty() {
        return None;
    }
    live.manager
        .account_selection(&account_id)
        .filter(AccountSelection::is_current)
}

/// The open session's selection while it is still the account on screen.
/// A departed selection clears the explorer.
fn live_session(ui: &AppWindow) -> Option<AccountSelection> {
    if !ui.get_discovery_context_allowed() || ui.get_room_actions_open() {
        clear(ui);
        return None;
    }
    let selection = SESSION.with(|session| {
        session
            .borrow()
            .as_ref()
            .map(|session| session.selection.clone())
    })?;
    let on_screen = ui.get_active_account_local_id().as_str() == selection.account_id();
    if selection.is_current() && on_screen {
        Some(selection)
    } else {
        clear(ui);
        None
    }
}

fn show(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    handle: &Handle,
    space_id: &str,
    name: &str,
    via: Vec<String>,
) {
    clear(ui);
    let Some(selection) = current_selection(ui, live) else {
        return;
    };
    if !space_id.starts_with('!') {
        return;
    }
    let explorer = Explorer::new(next_operation(), space_id, name, via);
    SESSION.with(|session| {
        *session.borrow_mut() = Some(Session {
            selection,
            explorer,
        });
    });
    crate::matrix_links::clear(ui);
    crate::thread_browser::clear(ui);
    ui.global::<SpaceExplorer>().set_open(true);
    step(ui, live, handle, Explorer::next_request);
}

fn navigate(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    handle: &Handle,
    change: impl FnOnce(&mut Explorer, u64) -> bool,
) {
    if live_session(ui).is_none() {
        return;
    }
    let changed = SESSION.with(|session| {
        let mut session = session.borrow_mut();
        let session = session.as_mut()?;
        Some(change(&mut session.explorer, next_operation()))
    });
    if changed == Some(true) {
        ui.global::<SpaceExplorer>().set_query("".into());
        // A level visited before keeps its rows; only a new one is read.
        step(ui, live, handle, |explorer| {
            (explorer.top().pages == 0)
                .then(|| explorer.next_request())
                .flatten()
        });
    } else {
        publish(ui);
    }
}

/// Ask for one page if `next` produces a request, then show the state.
fn step(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    handle: &Handle,
    next: impl FnOnce(&mut Explorer) -> Option<Request>,
) {
    let Some(selection) = live_session(ui) else {
        return;
    };
    let request = SESSION.with(|session| next(&mut session.borrow_mut().as_mut()?.explorer));
    publish(ui);
    let Some(request) = request else {
        return;
    };
    let manager = live.manager.clone();
    let weak = ui.as_weak();
    handle.spawn(async move {
        let read = manager.space_level_for(&selection, &request.space_id, request.from.as_deref());
        let result = tokio::time::timeout(PAGE_TIMEOUT, read)
            .await
            .unwrap_or(Err(SpaceLevelError::Unavailable));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            // Operation numbers are never reused, so a page for an earlier
            // explorer, level or selection is simply not waited for.
            let waiting = SESSION.with(|session| {
                let session = session.borrow();
                session
                    .as_ref()
                    .is_some_and(|session| session.explorer.waits_for(&request))
            });
            if !waiting {
                return;
            }
            let departed = matches!(result, Err(SpaceLevelError::AccountChanged));
            if departed || !selection.is_current() || live_session(&ui).is_none() {
                clear(&ui);
                return;
            }
            let applied = SESSION.with(|session| {
                let mut session = session.borrow_mut();
                let explorer = &mut session.as_mut()?.explorer;
                Some(explorer.apply(&request, result))
            });
            if applied == Some(true) {
                publish(&ui);
            }
        });
    });
}

fn join(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle, room_id: &str) {
    let Some(selection) = live_session(ui) else {
        return;
    };
    let ticket = next_operation();
    let target = SESSION.with(|session| {
        let mut session = session.borrow_mut();
        let explorer = &mut session.as_mut()?.explorer;
        let target = explorer.join_target(room_id)?;
        explorer.begin_join(room_id, ticket).then_some(target)
    });
    publish(ui);
    let Some(target) = target else {
        return;
    };
    // The navigation and security generations at this click.
    let asked = live.join_asked();
    JOINING.with(|joining| *joining.borrow_mut() = Some((ticket, asked.clone())));
    let (live, task) = (live.clone(), handle.clone());
    let weak = ui.as_weak();
    handle.spawn(async move {
        let link = target.link();
        let joining = live.join_target_for(&selection, &link, &asked, weak.clone());
        // This caller bound also covers inherited SDK metadata setup. SDK
        // admission conservatively marks uncertainty, not proof of wire IO.
        let result = tokio::time::timeout(JOIN_TIMEOUT, joining)
            .await
            .unwrap_or_else(|_| Err(asked.expired().into()));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            JOINING.with(|joining| {
                let mut joining = joining.borrow_mut();
                if joining.as_ref().is_some_and(|(owner, _)| *owner == ticket) {
                    *joining = None;
                }
            });
            if live_session(&ui).is_none() {
                return;
            }
            let failure = result
                .as_ref()
                .err()
                .map_or("", |error| target.failure(error));
            let shown_space = SESSION.with(|session| {
                let mut session = session.borrow_mut();
                let explorer = &mut session.as_mut()?.explorer;
                explorer
                    .finish_join(ticket, result.is_ok(), failure)
                    .then(|| explorer.top().space_id == target.room_id)
            });
            match shown_space {
                // The shown Space could not be previewed before it was
                // joined; read its first page now.
                Some(true) => step(&ui, &live, &task, |explorer| {
                    (explorer.top().pages == 0)
                        .then(|| explorer.next_request())
                        .flatten()
                }),
                Some(false) => publish(&ui),
                None => {}
            }
        });
    });
}

fn open(ui: &AppWindow, live: &LiveUi, room_id: &str) {
    if live_session(ui).is_none() || !live.is_live.load(Ordering::Acquire) {
        return;
    }
    let target = SESSION.with(|session| session.borrow().as_ref()?.explorer.open_target(room_id));
    let Some((room_id, name, is_space)) = target else {
        return;
    };
    clear(ui);
    ui.set_page(0);
    if is_space {
        ui.set_active_space_id(room_id.as_str().into());
        ui.set_active_space_name(name.into());
        ui.invoke_select_space(room_id.into());
    } else {
        ui.invoke_open_room(room_id.into());
    }
}

/// Show the explorer's state. Rows update in place, so a Load more keeps
/// the list's position and a status change keeps the focused row.
fn publish(ui: &AppWindow) {
    let state = ui.global::<SpaceExplorer>();
    SESSION.with(|session| {
        let session = session.borrow();
        let Some(session) = session.as_ref() else {
            return;
        };
        let explorer = &session.explorer;
        let level = explorer.top();
        let rows = explorer
            .visible()
            .map(|row| row_item(explorer, row))
            .collect::<Vec<_>>();
        ROWS.with(|model| sync(model, rows));
        let crumbs = explorer
            .trail
            .iter()
            .enumerate()
            .map(|(index, level)| SpaceExplorerCrumb {
                id: level.space_id.as_str().into(),
                name: level.name.as_str().into(),
                index: index as i32,
            })
            .collect::<Vec<_>>();
        CRUMBS.with(|model| sync(model, crumbs));
        state.set_title(level.name.as_str().into());
        state.set_space_id(level.space_id.as_str().into());
        state.set_summary(summary(explorer).into());
        state.set_space_status(
            match level.membership {
                Some(RoomMembership::Joined) => "You are in this Space",
                Some(RoomMembership::Invited) => "You are invited to this Space",
                Some(RoomMembership::Banned) => "You are banned from this Space",
                _ => "You have not joined this Space",
            }
            .into(),
        );
        state.set_error(level.error.map_or("", SpaceLevelError::text).into());
        state.set_notice(explorer.notice.as_str().into());
        state.set_loading(explorer.loading());
        state.set_can_load_more(explorer.can_load_more());
        state.set_can_open_space(level.membership == Some(RoomMembership::Joined));
        state.set_can_join_space(explorer.join_target(&level.space_id).is_some());
        state.set_joining_id(explorer.joining().unwrap_or("").into());
    });
}

fn summary(explorer: &Explorer) -> String {
    let level = explorer.top();
    let shown = explorer.visible().count();
    let mut text = if shown == level.rows.len() {
        format!("{} rooms and Spaces", level.rows.len())
    } else {
        format!("{shown} of {} loaded rooms match", level.rows.len())
    };
    if let Some(links) = &level.links
        && !level.complete
    {
        text.push_str(&format!(" · {} linked", links.len()));
    }
    if level.links_truncated || level.capped {
        text.push_str(" · more than can be shown");
    }
    text
}

fn row_item(explorer: &Explorer, row: &Row) -> SpaceExplorerRow {
    let status = row.status();
    let in_trail = row.is_space && explorer.in_trail(&row.room_id);
    let mut detail = Vec::new();
    if !row.alias.is_empty() {
        detail.push(row.alias.clone());
    }
    if let Some(links) = row.links {
        detail.push(format!("Space · {links} linked"));
    } else if !row.missing {
        detail.push(format!("{} members", row.members));
    }
    if row.suggested {
        detail.push("Suggested".into());
    }
    if in_trail {
        detail.push("Already shown above".into());
    }
    SpaceExplorerRow {
        id: row.room_id.as_str().into(),
        name: row.name.as_str().into(),
        initials: initials(&row.name),
        detail: detail.join(" · ").into(),
        topic: row.topic.as_str().into(),
        status: status.label().into(),
        is_space: row.is_space,
        joined: status == Status::Joined,
        can_open: status == Status::Joined && !row.unsupported,
        can_browse: row.is_space && !row.missing && (in_trail || explorer.trail.len() < MAX_TRAIL),
        can_join: status.joinable() && explorer.joining().is_none(),
        in_trail,
    }
}

fn initials(name: &str) -> SharedString {
    name.chars()
        .find(|ch| ch.is_alphanumeric())
        .map(|ch| ch.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "#".into())
        .into()
}

/// Make `model` hold `rows`, changing only the rows that differ.
fn sync<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    let shared = rows.len().min(model.row_count());
    for (index, row) in rows.iter().enumerate().take(shared) {
        if model.row_data(index).as_ref() != Some(row) {
            model.set_row_data(index, row.clone());
        }
    }
    while model.row_count() > rows.len() {
        model.remove(model.row_count() - 1);
    }
    for row in rows.into_iter().skip(shared) {
        model.push(row);
    }
}
