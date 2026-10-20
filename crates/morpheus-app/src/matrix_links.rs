//! Matrix links in messages, opened inside Morpheus.
//!
//! A click on a link in a message body, thread root or thread reply comes
//! here first (main.rs, open-message-link). A link that is not a Matrix
//! link keeps the external-link confirmation. Which links are Matrix links
//! (exact `https://matrix.to/#/` and plain `matrix:` shapes, bounded) is
//! `morpheus_matrix::message_link`; a Matrix link is never opened in the
//! browser or handed to the system by itself, and matrix.to is never asked.
//!
//! - A link opens for the account on screen at the click, under that exact
//!   `AccountSelection` and the click's navigation generation. It never
//!   switches accounts, and nothing it started navigates once a newer
//!   navigation, a newer link, Close or an account change came.
//! - What a room link names is read by `AccountManager::preview_link_for`:
//!   a room ID from this device's store, an alias by one bounded GET to the
//!   account's own homeserver. While that read runs past a short delay, the
//!   card says so and Close withdraws it. That first show happens only while
//!   the link still owns the screen: the same link, its account still on
//!   screen, nothing navigated since its click (a room or Space chosen
//!   meanwhile included) and no protected surface up; otherwise the read is
//!   withdrawn and nothing takes focus. A card actually shown before a later
//!   navigation then needs an explicit Open.
//! - A joined room opens through the app's owned open-conversation seam
//!   (message_search::open_conversation): revealed like a room-list click,
//!   a message scrolled to or opened in a focused view, in its thread when
//!   the local cache knows the thread; drafts are kept, and an edit being
//!   typed elsewhere refuses the open there. A joined Space opens like a
//!   sidebar click.
//! - Anything else shows the card (matrix-link.slint): the room ID or
//!   address as bounded plain text, what this device knows of it, and Join
//!   only where a join can apply. Joining is its own click, through the same
//!   bounded, withdrawable join as Join a room (LiveUi::join_target_for)
//!   with that click's navigation, security and account capture and its
//!   retained unknown outcome. Nothing joins, creates a direct message or
//!   sends anything because a link was clicked. A joined message link then
//!   shows its message only under the join's own receipt
//!   (`JoinAsked::owned_open`: the navigation the join's open took, the
//!   exact room it showed, no security step since): a navigation since the
//!   join (the same room chosen again, an account switched and back) shows
//!   nothing old, and the confirmed join stays reported as joined. Every
//!   later step of that message (each scroll retry, the focused view's
//!   commit, its first rows and its scroll) asks that navigation again and
//!   the follow-up's own authority (`JoinAsked::follow_up_authority`: the
//!   join's click-time security generation, its selection and the session),
//!   which the next Close, newer link, account change or any other surface
//!   withdraws at once: every modal and protected surface withdraws the link
//!   synchronously at its entry, and a step admitted while one is up ends
//!   it for good (`surface_blocked`), so closing that surface never revives
//!   it.
//! - A user link shows the user ID: people are not opened from links in
//!   this build (integration seam: root's People preview and explicit DM).
//! - An unsupported matrix.to link offers the browser only through the
//!   external-link confirmation; an unsupported `matrix:` URI offers nothing.
//! - Close, Escape, a newer link, an account change and any protected
//!   surface (sign-in, Settings, Manage accounts, account access, a
//!   recovery key, the session ending) withdraw the shown link before the
//!   card is hidden: its read is cancelled and its join withdrawn (nothing
//!   sent before the hand-off; after it nothing opens, and the outcome stays
//!   with LiveUi's join record). Every result checks its operation number,
//!   which each link and each clear advances, so none is shown afterwards.
//!
//! Nothing is logged; the link, IDs and names live only while shown, and
//! Slint values are built on the UI thread only.

use std::{
    cell::{Cell, RefCell},
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use morpheus_matrix::{
    AccountSelection, JoinCancellation, JoinError, LinkError, LinkPreview, LinkRoom,
    MatrixCoreError, MatrixLinkTarget, MessageLink, message_link,
};
use slint::{ComponentHandle, Timer};
use tokio::runtime::Handle;

use crate::{
    AppWindow, MatrixLink,
    live::{JoinAsked, JoinOpened, LiveUi, NavigationAuthority},
    message_search::{self, ConversationTarget},
};

/// A read still running after this long shows the card, so an already
/// joined room opens without one flashing.
const REVEAL_DELAY: Duration = Duration::from_millis(250);
/// Backstop over the read's own bounds (client wait, GET, cache read).
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Backstop over the join's own bounds, as the Space explorer's.
const JOIN_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_LABEL_CHARS: usize = 120;

const SIGNED_OUT: &str = "Sign in to open Matrix links.";
const READING_ROOM: &str = "Checking this room…";
const READING_ALIAS: &str = "Looking up this address on your homeserver…";
const NOT_JOINED: &str = "You have not joined this room.";
const LEFT: &str = "You left this room. Join it again to open it.";
const INVITED: &str = "You are invited to this room. Accept or decline it in the room list.";
const KNOCKED: &str = "You asked to join this room, and it has not let you in yet.";
const BANNED: &str = "You are banned from this room, so it cannot be joined from here.";
const MOVED_ON: &str = "You moved elsewhere since choosing this link. Open it now?";
const JOINING: &str = "Joining…";
const JOINED_NOT_SHOWN: &str =
    "Joined. Sync has not brought the room here yet; open it from the room list once it appears.";
const JOIN_UNCLEAR: &str = "Joining did not complete. Check your room list before joining again.";
const USER_LINK: &str =
    "This is a person's Matrix ID. People are not opened from links here yet. Nothing was sent.";
const UNSUPPORTED_WEB: &str =
    "Morpheus cannot open this kind of Matrix link. Your browser can, by asking matrix.to.";
const UNSUPPORTED_URI: &str =
    "Morpheus cannot open this Matrix link, and does not hand it to other apps.";

/// The one action a card offers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Action {
    #[default]
    None,
    Join,
    Open,
    Browser,
}

impl Action {
    fn code(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Join => 1,
            Self::Open => 2,
            Self::Browser => 3,
        }
    }
}

/// What the card shows, built from owned values off the UI thread's state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Card {
    title: &'static str,
    target: String,
    detail: String,
    status: String,
    error: bool,
    busy: bool,
    action: Action,
    /// The action's caption and accessible label.
    action_text: &'static str,
    action_label: String,
}

impl Card {
    fn with_action(mut self, action: Action, space: bool) -> Self {
        self.action = action;
        (self.action_text, self.action_label) = match action {
            Action::None => ("", String::new()),
            Action::Join if space => ("Join Space", format!("Join Space {}", self.target)),
            Action::Join => ("Join room", format!("Join room {}", self.target)),
            Action::Open => ("Open", format!("Open {}", self.target)),
            Action::Browser => (
                "Open in browser…",
                "Open this link in your browser, after confirming it".into(),
            ),
        };
        self
    }
}

/// Bounded plain text for a label: control and direction characters
/// removed.
fn label(text: &str) -> String {
    crate::directory_plain_text(text, MAX_LABEL_CHARS)
}

/// The card's title for a room link.
fn room_title(target: &MatrixLinkTarget, space: bool) -> &'static str {
    match target {
        MatrixLinkTarget::Room {
            event_id: Some(_), ..
        } => "Message link",
        MatrixLinkTarget::Room { .. } if space => "Space link",
        MatrixLinkTarget::Room { .. } => "Room link",
        MatrixLinkTarget::User { .. } => "User link",
    }
}

/// What a room link carries besides its room: a message, and how many
/// routing servers it names (their names are not shown).
fn detail(target: &MatrixLinkTarget) -> String {
    let MatrixLinkTarget::Room {
        event_id,
        via_servers,
        ..
    } = target
    else {
        return String::new();
    };
    let mut parts = Vec::new();
    if event_id.is_some() {
        parts.push("Points to a message".to_owned());
    }
    match via_servers.len() {
        0 => {}
        1 => parts.push("Names 1 routing server".into()),
        count => parts.push(format!("Names {count} routing servers")),
    }
    parts.join(" · ")
}

fn room_or_alias(target: &MatrixLinkTarget) -> &str {
    match target {
        MatrixLinkTarget::Room {
            room_or_alias_id, ..
        } => room_or_alias_id,
        MatrixLinkTarget::User { user_id } => user_id,
    }
}

/// A room link while it is read.
fn reading_card(target: &MatrixLinkTarget) -> Card {
    let alias = room_or_alias(target).starts_with('#');
    Card {
        title: room_title(target, false),
        target: label(room_or_alias(target)),
        detail: detail(target),
        status: if alias { READING_ALIAS } else { READING_ROOM }.into(),
        busy: true,
        ..Card::default()
    }
}

/// Where a read room link goes next.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Next {
    /// A joined room: open it, at its message when the link names one.
    Room(ConversationTarget),
    /// A joined Space: select it.
    Space { room_id: String, name: String },
    /// Show the card.
    Card(Card),
}

/// What a read room link leads to for `account_id`.
fn after_preview(account_id: &str, target: &MatrixLinkTarget, preview: &LinkPreview) -> Next {
    let MatrixLinkTarget::Room { event_id, .. } = target else {
        return Next::Card(user_card(room_or_alias(target)));
    };
    let shown = |status: &str, action: Action| {
        let card = Card {
            title: room_title(target, false),
            target: label(room_or_alias(target)),
            detail: detail(target),
            status: status.into(),
            ..Card::default()
        };
        Next::Card(card.with_action(action, false))
    };
    match preview.room {
        LinkRoom::Joined { is_space: true } => Next::Space {
            room_id: preview.room_id.clone(),
            name: label(preview.name.as_deref().unwrap_or(&preview.room_id)),
        },
        LinkRoom::Joined { is_space: false } => Next::Room(ConversationTarget {
            account_id: account_id.to_owned(),
            room_id: preview.room_id.clone(),
            event_id: event_id.clone(),
            thread_root: preview.thread_root.clone(),
        }),
        LinkRoom::Unknown => shown(NOT_JOINED, Action::Join),
        LinkRoom::Left => shown(LEFT, Action::Join),
        LinkRoom::Invited => shown(INVITED, Action::None),
        LinkRoom::Knocked => shown(KNOCKED, Action::None),
        LinkRoom::Banned => shown(BANNED, Action::None),
    }
}

/// A joined target whose click's navigation is no longer current: opening
/// it needs a new, explicit Open.
fn moved_on_card(target: &MatrixLinkTarget, space: bool) -> Card {
    Card {
        title: room_title(target, space),
        target: label(room_or_alias(target)),
        detail: detail(target),
        status: MOVED_ON.into(),
        ..Card::default()
    }
    .with_action(Action::Open, space)
}

/// A room link that could not be read. Join is offered only where the join
/// itself may still find the room (it resolves an address on its own).
fn failure_card(target: &MatrixLinkTarget, error: LinkError) -> Card {
    let joinable = matches!(
        error,
        LinkError::RateLimited | LinkError::NoAnswer | LinkError::Unavailable
    );
    Card {
        title: room_title(target, false),
        target: label(room_or_alias(target)),
        detail: detail(target),
        status: error.to_string(),
        error: true,
        ..Card::default()
    }
    .with_action(if joinable { Action::Join } else { Action::None }, false)
}

/// What a finished join of a link leaves on the card, when the card stays.
/// `None`: the room was shown, so the card closes.
fn joined_card(target: &MatrixLinkTarget, result: &Result<bool, MatrixCoreError>) -> Option<Card> {
    let card = Card {
        title: room_title(target, false),
        target: label(room_or_alias(target)),
        detail: detail(target),
        ..Card::default()
    };
    match result {
        Ok(true) => None,
        Ok(false) => Some(Card {
            status: JOINED_NOT_SHOWN.into(),
            ..card
        }),
        Err(MatrixCoreError::Join(error)) => {
            // A definite refusal or a join never sent may be tried again; an
            // unknown outcome, or one paused by its record, may not.
            let again = !error.may_have_joined()
                && !matches!(
                    error,
                    JoinError::EarlierUnknown | JoinError::Pending | JoinError::UnresolvedLimit
                );
            let card = Card {
                status: error.to_string(),
                error: true,
                ..card
            };
            Some(card.with_action(if again { Action::Join } else { Action::None }, false))
        }
        Err(_) => Some(Card {
            status: JOIN_UNCLEAR.into(),
            error: true,
            ..card
        }),
    }
}

fn user_card(user_id: &str) -> Card {
    Card {
        title: "User link",
        target: label(user_id),
        status: USER_LINK.into(),
        ..Card::default()
    }
}

fn unsupported_card(href: &str, web: bool) -> Card {
    let card = Card {
        title: "Matrix link",
        target: label(href),
        status: if web {
            UNSUPPORTED_WEB
        } else {
            UNSUPPORTED_URI
        }
        .into(),
        ..Card::default()
    };
    card.with_action(if web { Action::Browser } else { Action::None }, false)
}

fn signed_out_card() -> Card {
    Card {
        title: "Matrix link",
        status: SIGNED_OUT.into(),
        ..Card::default()
    }
}

/// The link a join sends: the link's own room or address and routes, or
/// the server of its ID when it names none (as Join a room does).
fn join_link(target: &MatrixLinkTarget) -> MatrixLinkTarget {
    let mut link = target.clone();
    if let MatrixLinkTarget::Room {
        room_or_alias_id,
        via_servers,
        ..
    } = &mut link
        && via_servers.is_empty()
        && let Some(server) = crate::matrix_id_server(room_or_alias_id.as_str())
    {
        via_servers.push(server.to_owned());
    }
    link
}

/// The shown link and what it may still do.
struct Session {
    operation: u64,
    action: Action,
    shown: Shown,
}

enum Shown {
    /// A card with nothing to read or act on (user, unsupported URI,
    /// signed out).
    Static,
    /// An unsupported matrix.to link the browser may open, only through the
    /// external-link confirmation.
    Web(String),
    Room(RoomLink),
}

struct RoomLink {
    selection: AccountSelection,
    /// The navigation generation at the link's click.
    navigation: u64,
    target: MatrixLinkTarget,
    /// Withdraws the read.
    reading: JoinCancellation,
    preview: Option<LinkPreview>,
    /// The join from this card, with its own click-time capture.
    joining: Option<JoinAsked>,
    /// Where an explicit Open goes, once the read showed a joined target.
    open: Option<Next>,
    /// The card was actually shown for this link (not only filled in).
    presented: bool,
}

/// What the first-show timer of a room link still being read does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FirstShow {
    Show,
    /// The link no longer owns the screen: withdraw it, moving no focus.
    Withdraw,
    /// Another link, or this one already shown or read: nothing.
    NotOurs,
}

/// The first show of a room link still being read: `ours` (the same link,
/// unread, not shown yet), its account on screen, nothing navigated since
/// its click, no protected surface up.
fn first_show(ours: bool, on_screen: bool, navigation_current: bool, blocked: bool) -> FirstShow {
    if !ours {
        FirstShow::NotOurs
    } else if on_screen && navigation_current && !blocked {
        FirstShow::Show
    } else {
        FirstShow::Withdraw
    }
}

/// Whether a finished read ends its link with nothing more: its account is
/// gone from screen, a protected surface is up, or something navigated
/// before the card was ever shown.
fn read_ends(on_screen: bool, blocked: bool, moved_on: bool, presented: bool) -> bool {
    !on_screen || blocked || (moved_on && !presented)
}

/// A joined message link's message, to show under the join's own receipt
/// (`opened`): in exactly the room the join showed, for the link's account,
/// under the navigation that open took.
fn event_follow_up(
    account_id: &str,
    target: &MatrixLinkTarget,
    opened: Option<JoinOpened>,
) -> Option<(ConversationTarget, u64)> {
    let MatrixLinkTarget::Room {
        event_id: Some(event_id),
        ..
    } = target
    else {
        return None;
    };
    let opened = opened?;
    let conversation = ConversationTarget {
        account_id: account_id.to_owned(),
        room_id: opened.room_id,
        event_id: Some(event_id.clone()),
        thread_root: None,
    };
    Some((conversation, opened.navigation))
}

thread_local! {
    // Slint callbacks and every publication run on the UI thread only.
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    // Each link and each clear takes a new number; a result for an older
    // one is never shown.
    static OPERATION: Cell<u64> = const { Cell::new(0) };
    // The authority of the last joined link's message follow-up, which
    // outlives its card: the next clear (Close, a newer link, an account
    // change, a protected surface) withdraws it at once.
    static FOLLOW_UP: RefCell<Option<NavigationAuthority>> = const { RefCell::new(None) };
}

fn next_operation() -> u64 {
    OPERATION.with(|operation| {
        let next = operation.get().wrapping_add(1);
        operation.set(next);
        next
    })
}

/// End the shown link: withdraw its read, its join and the last joined
/// link's follow-up first, then hide the card and forget everything it
/// showed.
pub(crate) fn clear(ui: &AppWindow) {
    next_operation();
    if let Some(follow_up) = FOLLOW_UP.with(|follow_up| follow_up.borrow_mut().take()) {
        follow_up.withdraw();
    }
    if let Some(Session {
        shown: Shown::Room(room),
        ..
    }) = SESSION.with(|session| session.borrow_mut().take())
    {
        room.reading.cancel();
        if let Some(asked) = room.joining {
            // Before its hand-off nothing is sent; after it nothing opens,
            // and its outcome stays with LiveUi's join record.
            asked.withdraw();
        }
    }
    let state = ui.global::<MatrixLink>();
    state.set_open(false);
    state.set_title("".into());
    state.set_target("".into());
    state.set_detail("".into());
    state.set_status("".into());
    state.set_status_error(false);
    state.set_busy(false);
    state.set_action(0);
    state.set_action_text("".into());
    state.set_action_label("".into());
}

pub(crate) fn is_open(ui: &AppWindow) -> bool {
    ui.global::<MatrixLink>().get_open()
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, handle: Handle) {
    let state = ui.global::<MatrixLink>();
    let weak = ui.as_weak();
    state.on_act(move || {
        if let Some(ui) = weak.upgrade() {
            act(&ui, &live, &handle);
        }
    });
    // Close or Escape: focus goes back to the conversation's composer once
    // the card is gone.
    let weak = ui.as_weak();
    state.on_close(move || {
        if let Some(ui) = weak.upgrade() {
            let shown = is_open(&ui);
            clear(&ui);
            if shown && !surface_blocked(&ui) && !ui.get_active_room_id().is_empty() {
                ui.invoke_focus_composer(false);
            }
        }
    });
    // A protected surface took over: nothing else is touched.
    let weak = ui.as_weak();
    state.on_withdraw(move || {
        if let Some(ui) = weak.upgrade() {
            clear(&ui);
        }
    });
}

/// Another surface is up, or the conversation is not on screen: a link
/// click does nothing then, a shown link is withdrawn, and a joined link's
/// message still to come is ended for good (its follow-up authority is
/// `blocked_by` this). It reads app-window's matrix-link-blocked, so Rust
/// and Slint use the one complete list: sign-in, any page but the
/// conversation, the account and security surfaces (access, Manage
/// accounts, the account menu, a pending switch, security work, a recovery
/// key, verification) and every modal (profile, People, member details, the
/// Space explorer and Space rooms, room addresses, room notifications, Seen
/// by, the quick switcher, room actions, room administration with its
/// permissions and redaction, member actions, the capture picker, leaving a
/// call, the image preview, polls, the thread browser). Each of their
/// entries also withdraws the link synchronously first (clear). The session ending is the link
/// modal's own exception: its signed-out card may still show, while the
/// follow-up's authority ends with the session it captured. Integration
/// seam: root's modal guard registry, once there is one, replaces the list.
fn surface_blocked(ui: &AppWindow) -> bool {
    ui.get_matrix_link_blocked()
}

/// The selection of the account on screen, if it may still read.
fn current_selection(ui: &AppWindow, live: &LiveUi) -> Option<AccountSelection> {
    if !live.is_live.load(Ordering::Acquire) || !ui.get_live_session() {
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

/// The room link's selection while it is still the account on screen.
fn on_screen(ui: &AppWindow, live: &LiveUi, selection: &AccountSelection) -> bool {
    selection.is_current()
        && live.is_live.load(Ordering::Acquire)
        && ui.get_live_session()
        && ui.get_active_account_local_id().as_str() == selection.account_id()
}

/// Show `card` for the current session, and open the card.
fn show(ui: &AppWindow, card: &Card) {
    if surface_blocked(ui) {
        clear(ui);
        return;
    }
    SESSION.with(|session| {
        if let Some(session) = session.borrow_mut().as_mut() {
            session.action = card.action;
            if let Shown::Room(room) = &mut session.shown {
                room.presented = true;
            }
        }
    });
    publish(ui, card);
    ui.global::<MatrixLink>().set_open(true);
}

fn publish(ui: &AppWindow, card: &Card) {
    let state = ui.global::<MatrixLink>();
    state.set_title(card.title.into());
    state.set_target(card.target.as_str().into());
    state.set_detail(card.detail.as_str().into());
    state.set_status(card.status.as_str().into());
    state.set_status_error(card.error);
    state.set_busy(card.busy);
    state.set_action(card.action.code());
    state.set_action_text(card.action_text.into());
    state.set_action_label(card.action_label.as_str().into());
}

/// A link clicked in a message. False when it is not a Matrix link, so the
/// caller keeps its external-link confirmation; true when it was handled
/// here, whatever came of it.
pub(crate) fn open(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle, href: &str) -> bool {
    let Some(link) = message_link(href) else {
        return false;
    };
    // One link at a time: the shown one is withdrawn first.
    clear(ui);
    if surface_blocked(ui) {
        return true;
    }
    // A card opens now: the thread browser (a modal that blocks links while
    // it is open) ends first, with any page or retry it still holds.
    crate::thread_browser::clear(ui);
    let operation = next_operation();
    let (shown, card) = match link {
        MessageLink::Unsupported { web: true } => {
            (Shown::Web(href.to_owned()), unsupported_card(href, true))
        }
        MessageLink::Unsupported { web: false } => (Shown::Static, unsupported_card(href, false)),
        MessageLink::Native(MatrixLinkTarget::User { user_id }) => {
            (Shown::Static, user_card(&user_id))
        }
        MessageLink::Native(target) => {
            read(ui, live, handle, operation, target);
            return true;
        }
    };
    SESSION.with(|session| {
        *session.borrow_mut() = Some(Session {
            operation,
            action: Action::None,
            shown,
        });
    });
    show(ui, &card);
    true
}

/// Read a room link for the account on screen; the card appears only if the
/// read is still running after REVEAL_DELAY.
fn read(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    handle: &Handle,
    operation: u64,
    target: MatrixLinkTarget,
) {
    let Some(selection) = current_selection(ui, live) else {
        SESSION.with(|session| {
            *session.borrow_mut() = Some(Session {
                operation,
                action: Action::None,
                shown: Shown::Static,
            });
        });
        show(ui, &signed_out_card());
        return;
    };
    let reading = JoinCancellation::new();
    let card = reading_card(&target);
    // The click's navigation, captured once: the timer and the read's
    // completion both check against it.
    let navigation = live.current_navigation_epoch();
    SESSION.with(|session| {
        *session.borrow_mut() = Some(Session {
            operation,
            action: Action::None,
            shown: Shown::Room(RoomLink {
                selection: selection.clone(),
                navigation,
                target: target.clone(),
                reading: reading.clone(),
                preview: None,
                joining: None,
                open: None,
                presented: false,
            }),
        });
    });
    publish(ui, &card);
    let weak = ui.as_weak();
    let (timer_live, timer_selection) = (live.clone(), selection.clone());
    Timer::single_shot(REVEAL_DELAY, move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        reveal_reading(
            &ui,
            &timer_live,
            operation,
            &timer_selection,
            navigation,
            &card,
        );
    });
    let (manager, weak) = (live.manager.clone(), ui.as_weak());
    let (live, handle) = (live.clone(), handle.clone());
    handle.clone().spawn(async move {
        let previewing = manager.preview_link_for(&selection, &target, &reading);
        let result = tokio::time::timeout(READ_TIMEOUT, previewing)
            .await
            .unwrap_or(Err(LinkError::Unavailable));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            finish_read(&ui, &live, &handle, operation, result);
        });
    });
}

/// The first show of a room link still being read, REVEAL_DELAY after its
/// click (`first_show`). A link that no longer owns the screen is withdrawn
/// (its read cancelled, the card left hidden), and nothing takes focus.
fn reveal_reading(
    ui: &AppWindow,
    live: &LiveUi,
    operation: u64,
    selection: &AccountSelection,
    navigation: u64,
    card: &Card,
) {
    let ours = SESSION.with(|session| {
        session.borrow().as_ref().is_some_and(|session| {
            session.operation == operation
                && matches!(
                    &session.shown,
                    Shown::Room(room) if room.preview.is_none() && !room.presented
                )
        })
    });
    let decided = first_show(
        ours,
        on_screen(ui, live, selection),
        live.navigation_is_current(navigation),
        surface_blocked(ui),
    );
    match decided {
        FirstShow::NotOurs => {}
        FirstShow::Withdraw => clear(ui),
        FirstShow::Show => show(ui, card),
    }
}

fn finish_read(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    handle: &Handle,
    operation: u64,
    result: Result<LinkPreview, LinkError>,
) {
    // Only the link still shown: a newer link, Close or a clear withdrew
    // any other.
    let room = SESSION.with(|session| {
        let session = session.borrow();
        match session.as_ref() {
            Some(Session {
                operation: owner,
                shown: Shown::Room(room),
                ..
            }) if *owner == operation => Some((
                room.selection.clone(),
                room.navigation,
                room.target.clone(),
                room.presented,
            )),
            _ => None,
        }
    });
    let Some((selection, navigation, target, presented)) = room else {
        return;
    };
    let moved_on = !live.navigation_is_current(navigation);
    // Gone from screen, covered, or the person moved on before anything of
    // this link was shown: nothing more happens.
    if read_ends(
        on_screen(ui, live, &selection),
        surface_blocked(ui),
        moved_on,
        presented,
    ) {
        clear(ui);
        return;
    }
    let preview = match result {
        Ok(preview) => preview,
        Err(LinkError::Cancelled) => return,
        Err(LinkError::AccountChanged) => {
            clear(ui);
            return;
        }
        Err(error) => {
            show(ui, &failure_card(&target, error));
            return;
        }
    };
    let next = after_preview(selection.account_id(), &target, &preview);
    SESSION.with(|session| {
        if let Some(Session {
            shown: Shown::Room(room),
            ..
        }) = session.borrow_mut().as_mut()
        {
            room.preview = Some(preview);
        }
    });
    if let Next::Card(card) = &next {
        show(ui, card);
        return;
    }
    if !moved_on {
        go(ui, live, handle, next);
        return;
    }
    // The card is up and something else navigated meanwhile: opening needs
    // a new, explicit choice.
    let space = matches!(next, Next::Space { .. });
    SESSION.with(|session| {
        if let Some(Session {
            shown: Shown::Room(room),
            ..
        }) = session.borrow_mut().as_mut()
        {
            room.open = Some(next);
        }
    });
    show(ui, &moved_on_card(&target, space));
}

/// Close the card, then open a joined room or Space for the account on
/// screen, in the same UI turn.
fn go(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle, next: Next) {
    clear(ui);
    match next {
        Next::Room(conversation) => {
            message_search::open_conversation(ui, live, handle, conversation)
        }
        Next::Space { room_id, name } => {
            ui.set_active_space_id(room_id.as_str().into());
            ui.set_active_space_name(name.into());
            ui.invoke_select_space(room_id.into());
        }
        Next::Card(_) => {}
    }
}

/// The card's one action.
fn act(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {
    let action = SESSION.with(|session| {
        session
            .borrow()
            .as_ref()
            .map_or(Action::None, |session| session.action)
    });
    match action {
        Action::None => {}
        Action::Join => join(ui, live, handle),
        Action::Open => open_chosen(ui, live, handle),
        Action::Browser => browse(ui),
    }
}

/// An explicit Open of a joined target whose click's navigation had moved:
/// this click is the new navigation.
fn open_chosen(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {
    let chosen = SESSION.with(|session| {
        let mut session = session.borrow_mut();
        let Some(Session {
            shown: Shown::Room(room),
            ..
        }) = session.as_mut()
        else {
            return None;
        };
        Some((room.selection.clone(), room.open.take()?))
    });
    let Some((selection, next)) = chosen else {
        return;
    };
    if !on_screen(ui, live, &selection) {
        clear(ui);
        return;
    }
    go(ui, live, handle, next);
}

/// Hand an unsupported matrix.to link to the external-link confirmation.
fn browse(ui: &AppWindow) {
    let href = SESSION.with(|session| match session.borrow().as_ref() {
        Some(Session {
            shown: Shown::Web(href),
            ..
        }) => Some(href.clone()),
        _ => None,
    });
    clear(ui);
    if let Some(href) = href {
        crate::confirm_external_link(&href);
    }
}

/// Join the shown room link: an explicit click, with this click's own
/// capture (LiveUi::join_target_for).
fn join(ui: &AppWindow, live: &Arc<LiveUi>, handle: &Handle) {
    let prepared = SESSION.with(|session| {
        let session = session.borrow();
        match session.as_ref() {
            Some(Session {
                operation,
                action: Action::Join,
                shown: Shown::Room(room),
            }) if room.joining.is_none() => {
                Some((*operation, room.selection.clone(), room.target.clone()))
            }
            _ => None,
        }
    });
    let Some((operation, selection, target)) = prepared else {
        return;
    };
    if !on_screen(ui, live, &selection) {
        clear(ui);
        return;
    }
    // The navigation and security generations at this click.
    let asked = live.join_asked();
    SESSION.with(|session| {
        if let Some(Session {
            action,
            shown: Shown::Room(room),
            ..
        }) = session.borrow_mut().as_mut()
        {
            *action = Action::None;
            room.joining = Some(asked.clone());
        }
    });
    let card = Card {
        status: JOINING.into(),
        busy: true,
        ..reading_card(&target)
    };
    show(ui, &card);
    let (live, handle) = (live.clone(), handle.clone());
    let weak = ui.as_weak();
    handle.clone().spawn(async move {
        let link = join_link(&target);
        let joining = live.join_target_for(&selection, &link, &asked, weak.clone());
        // Every wait inside is bounded; past this backstop the request may
        // have been sent only if it was handed off.
        let result = tokio::time::timeout(JOIN_TIMEOUT, joining)
            .await
            .unwrap_or_else(|_| Err(asked.expired().into()));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            finish_join(&ui, &live, &handle, operation, &asked, result);
        });
    });
}

/// The join's queued completion, after its last await: only for the same
/// link (`operation`) whose account is still on screen. A message link's
/// message then opens only under the join's own receipt (`asked`), checked
/// here on the UI thread.
fn finish_join(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    handle: &Handle,
    operation: u64,
    asked: &JoinAsked,
    result: Result<bool, MatrixCoreError>,
) {
    // Closed or replaced meanwhile: its outcome stays with the join record.
    let room = SESSION.with(|session| {
        let mut session = session.borrow_mut();
        match session.as_mut() {
            Some(Session {
                operation: owner,
                shown: Shown::Room(room),
                ..
            }) if *owner == operation => {
                room.joining = None;
                Some((room.selection.clone(), room.target.clone()))
            }
            _ => None,
        }
    });
    let Some((selection, target)) = room else {
        return;
    };
    if !on_screen(ui, live, &selection) {
        clear(ui);
        return;
    }
    if let Some(card) = joined_card(&target, &result) {
        show(ui, &card);
        return;
    }
    // Joined and shown under the join's own authority. A message link's
    // message opens in exactly that room, under that open's navigation,
    // only while it still owns the screen: never under a navigation taken
    // since, and whatever happens, the join stays joined.
    let account_id = selection.account_id().to_owned();
    let follow_up = event_follow_up(&account_id, &target, asked.owned_open(live));
    clear(ui);
    let Some((conversation, navigation)) = follow_up else {
        return;
    };
    if surface_blocked(ui) {
        return;
    }
    // Its own authority keeps the join's click-time security, selection and
    // session to the end (every scroll retry, the focused view's commit and
    // its first rows), any of those steps admitted while another surface is
    // up ends it for good, and this module withdraws it at the next clear.
    let authority = asked
        .follow_up_authority(live, &selection)
        .blocked_by(surface_blocked);
    FOLLOW_UP.with(|follow_up| *follow_up.borrow_mut() = Some(authority.clone()));
    message_search::continue_conversation(
        ui,
        live,
        handle,
        selection,
        conversation,
        navigation,
        authority,
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use slint::platform::{Key, WindowEvent};

    fn room(id: &str, event: Option<&str>, via: &[&str]) -> MatrixLinkTarget {
        MatrixLinkTarget::Room {
            room_or_alias_id: id.into(),
            event_id: event.map(Into::into),
            via_servers: via.iter().map(|server| (*server).to_owned()).collect(),
        }
    }

    fn preview(room: LinkRoom) -> LinkPreview {
        LinkPreview {
            room_id: "!room:example.org".into(),
            room,
            name: Some("Lobby\u{202e}".into()),
            thread_root: Some("$root:example.org".into()),
        }
    }

    #[test]
    fn a_joined_room_opens_at_its_message_and_thread_and_a_space_selects() {
        let target = room(
            "#lobby:example.org",
            Some("$event:example.org"),
            &["one.org"],
        );
        let joined = preview(LinkRoom::Joined { is_space: false });
        let next = after_preview("work", &target, &joined);
        assert_eq!(
            next,
            Next::Room(ConversationTarget {
                account_id: "work".into(),
                room_id: "!room:example.org".into(),
                event_id: Some("$event:example.org".into()),
                thread_root: Some("$root:example.org".into()),
            }),
            "the alias's room, never the alias itself, opens for the exact account"
        );
        let joined_space = preview(LinkRoom::Joined { is_space: true });
        let space = after_preview("work", &target, &joined_space);
        let Next::Space { room_id, name } = space else {
            panic!("a joined Space is selected");
        };
        assert_eq!(room_id, "!room:example.org");
        assert!(!name.contains('\u{202e}'), "names are plain text");
    }

    #[test]
    fn nothing_is_joined_without_its_own_click_and_only_where_a_join_applies() {
        let target = room("!room:example.org", None, &[]);
        let action = |membership| match after_preview("work", &target, &preview(membership)) {
            Next::Card(card) => card.action,
            other => panic!("{other:?}"),
        };
        assert_eq!(action(LinkRoom::Unknown), Action::Join);
        assert_eq!(action(LinkRoom::Left), Action::Join);
        // An invitation is answered in the room list; a knock or a ban is
        // not joined from a link.
        assert_eq!(action(LinkRoom::Invited), Action::None);
        assert_eq!(action(LinkRoom::Knocked), Action::None);
        assert_eq!(action(LinkRoom::Banned), Action::None);
        // A failed read offers Join only where the join may still find the
        // room by itself.
        for (error, joinable) in [
            (LinkError::NoAnswer, true),
            (LinkError::RateLimited, true),
            (LinkError::Unavailable, true),
            (LinkError::AliasNotFound, false),
            (LinkError::Refused, false),
            (LinkError::InvalidTarget, false),
        ] {
            let card = failure_card(&target, error);
            assert_eq!(card.action == Action::Join, joinable, "{error:?}");
            assert!(card.error && card.status == error.to_string());
        }
    }

    #[test]
    fn a_join_reports_only_what_happened_and_never_offers_a_blind_resend() {
        let target = room("!room:example.org", None, &[]);
        // Shown: the card closes.
        assert_eq!(joined_card(&target, &Ok(true)), None);
        // Joined but not here yet: said, with nothing to resend.
        let unsynced = joined_card(&target, &Ok(false)).unwrap();
        assert_eq!(unsynced.status, JOINED_NOT_SHOWN);
        assert_eq!(unsynced.action, Action::None);
        for (error, again) in [
            (JoinError::NotAllowed, true),
            (JoinError::RateLimited, true),
            (JoinError::Cancelled, true),
            (JoinError::Blocked, true),
            (JoinError::OutcomeUnknown, false),
            (JoinError::UnexpectedRoom, false),
            (JoinError::EarlierUnknown, false),
            (JoinError::Pending, false),
            (JoinError::UnresolvedLimit, false),
        ] {
            let card = joined_card(&target, &Err(MatrixCoreError::Join(error))).unwrap();
            assert_eq!(card.action == Action::Join, again, "{error:?}");
            assert_eq!(card.status, error.to_string(), "fixed sentences only");
        }
        let other = joined_card(&target, &Err(MatrixCoreError::Sdk("secret".into()))).unwrap();
        assert_eq!(other.status, JOIN_UNCLEAR);
        assert!(!other.status.contains("secret"));
    }

    #[test]
    fn cards_show_bounded_plain_labels_and_counts_not_route_names() {
        let long = format!("#\u{202e}{}:example.org", "a".repeat(300));
        let target = room(&long, Some("$event:example.org"), &["one.org", "two.org"]);
        let card = reading_card(&target);
        assert!(card.busy && card.status == READING_ALIAS);
        assert!(card.target.chars().count() <= MAX_LABEL_CHARS);
        assert!(!card.target.contains('\u{202e}'));
        assert_eq!(card.title, "Message link");
        assert_eq!(card.detail, "Points to a message · Names 2 routing servers");
        assert!(!card.detail.contains("one.org"));
        let by_id = reading_card(&room("!r:example.org", None, &[]));
        assert_eq!(by_id.status, READING_ROOM);
    }

    #[test]
    fn users_and_unsupported_links_never_act_outside_morpheus_by_themselves() {
        let user = user_card("@alice:example.org");
        assert_eq!(user.action, Action::None);
        assert!(user.status.contains("Nothing was sent"));
        // Only a matrix.to link may go to the browser, and only through the
        // external-link confirmation; a matrix: URI goes nowhere.
        let web = unsupported_card("https://matrix.to/#/+group:example.org", true);
        assert_eq!(web.action, Action::Browser);
        let uri = unsupported_card("matrix://attacker.example/r/x", false);
        assert_eq!(uri.action, Action::None);
        assert_eq!(signed_out_card().action, Action::None);
        // A joined target whose navigation moved needs an explicit Open.
        let moved = moved_on_card(&room("!r:example.org", None, &[]), false);
        assert_eq!(moved.action, Action::Open);
        assert_eq!(moved.status, MOVED_ON);
    }

    #[test]
    fn a_join_names_the_link_s_routes_or_its_own_server() {
        let routed = room("!room:example.org", Some("$e:example.org"), &["one.org"]);
        assert_eq!(join_link(&routed), routed);
        let bare = room("!room:example.org", None, &[]);
        let own_server = room("!room:example.org", None, &["example.org"]);
        assert_eq!(join_link(&bare), own_server);
        let alias = room("#lobby:other.org", None, &[]);
        let alias_server = room("#lobby:other.org", None, &["other.org"]);
        assert_eq!(join_link(&alias), alias_server);
    }

    /// The first show after REVEAL_DELAY needs the link to still own the
    /// screen; a stale hidden read is withdrawn, never shown or focused, and
    /// a newer link is left alone. Its completion then does nothing more,
    /// unless the card was actually shown before a later navigation (then an
    /// explicit Open).
    #[test]
    fn a_read_shows_its_card_only_while_it_still_owns_the_screen() {
        use FirstShow::{NotOurs, Show, Withdraw};
        assert_eq!(first_show(true, true, true, false), Show);
        // A room or Space chosen before the delay.
        assert_eq!(first_show(true, true, false, false), Withdraw);
        // Its account left the screen: a switch, or A to B and back to A.
        assert_eq!(first_show(true, false, true, false), Withdraw);
        // A protected surface took over.
        assert_eq!(first_show(true, true, true, true), Withdraw);
        // A newer link, or this one already read or shown.
        assert_eq!(first_show(false, true, true, false), NotOurs);
        assert_eq!(first_show(false, false, false, true), NotOurs);
        // Never shown before the navigation: the completion ends it.
        assert!(read_ends(true, false, true, false));
        // Shown first, then a navigation: it stays, asking for Open.
        assert!(!read_ends(true, false, true, true));
        assert!(!read_ends(true, false, false, false));
        assert!(read_ends(false, false, false, true));
        assert!(read_ends(true, true, false, true));
    }

    /// A joined message link's message opens only under the join's own
    /// receipt: its navigation (never a new one) and its exact room.
    #[test]
    fn a_joined_message_opens_only_under_the_join_s_own_receipt() {
        let link = room("#lobby:example.org", Some("$event:example.org"), &[]);
        let receipt = || JoinOpened {
            navigation: 9,
            room_id: "!room:example.org".into(),
        };
        let followed = event_follow_up("work", &link, Some(receipt()));
        let (conversation, navigation) = followed.expect("an unchanged owner opens it");
        assert_eq!(navigation, 9, "the join's own navigation");
        let expected = ConversationTarget {
            account_id: "work".into(),
            room_id: "!room:example.org".into(),
            event_id: Some("$event:example.org".into()),
            thread_root: None,
        };
        assert_eq!(conversation, expected);
        // No receipt (a newer navigation, a switch, a security step): no
        // old message, thread or focus.
        assert_eq!(event_follow_up("work", &link, None), None);
        // A room link names no message: nothing to follow up.
        let bare = room("!room:example.org", None, &[]);
        assert_eq!(event_follow_up("work", &bare, Some(receipt())), None);
    }

    #[test]
    fn action_codes_match_the_card() {
        let codes = [Action::None, Action::Join, Action::Open, Action::Browser].map(Action::code);
        assert_eq!(codes, [0, 1, 2, 3]);
        let card = Card {
            target: "#lobby:example.org".into(),
            ..Card::default()
        };
        let join = card.clone().with_action(Action::Join, true);
        assert_eq!(join.action_text, "Join Space");
        assert_eq!(join.action_label, "Join Space #lobby:example.org");
        assert_eq!(card.with_action(Action::None, false).action_text, "");
    }

    /// An event-loop barrier, then the conditional card's init and change
    /// handlers, as the UI suite settles.
    async fn settle(ui: &AppWindow) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        ui.as_weak()
            .upgrade_in_event_loop(move |_| {
                let _ = sender.send(());
            })
            .unwrap();
        receiver.await.unwrap();
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    }

    async fn press(ui: &AppWindow, key: Key) {
        let text: slint::SharedString = key.into();
        ui.window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        ui.window()
            .dispatch_event(WindowEvent::KeyReleased { text });
        settle(ui).await;
    }

    /// The real window (UI suite phase 83): a link that is not a Matrix link
    /// is left to the external-link confirmation; cards open with no action
    /// where none applies; Tab stays on the card; Close and Escape withdraw,
    /// hide and forget it; a protected surface withdraws it without a click;
    /// a link clicked under another surface does nothing; and a joined
    /// link's message still to come ends at a modal's own entry and never
    /// comes back when that modal closes. No account, request or browser is
    /// involved.
    pub(crate) async fn assert_link_card(ui: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
        register(ui, live.clone(), runtime.clone());
        let state = ui.global::<MatrixLink>();
        let click = |href: &str| open(ui, live, runtime, href);
        let (live_session, page) = (ui.get_live_session(), ui.get_page());
        let sign_in = ui.get_show_sign_in();
        ui.set_show_sign_in(false);
        ui.set_page(0);
        ui.set_live_session(false);
        settle(ui).await;
        assert!(!click("https://example.org/page"));
        assert!(!is_open(ui));
        // Signed out: a room link names nothing, and offers nothing.
        assert!(click("https://matrix.to/#/!room:example.org"));
        settle(ui).await;
        assert!(is_open(ui));
        assert_eq!(state.get_status().as_str(), SIGNED_OUT);
        assert_eq!(state.get_action(), 0);
        // Tab never leaves the card: Return still activates its Close.
        press(ui, Key::Tab).await;
        press(ui, Key::Return).await;
        assert!(!is_open(ui), "focus stayed on the card's Close");
        assert!(state.get_status().is_empty(), "nothing it showed is kept");
        // A person's link offers nothing; Escape withdraws and hides it.
        assert!(click("matrix:u/alice:example.org"));
        settle(ui).await;
        assert_eq!(state.get_title().as_str(), "User link");
        assert_eq!(state.get_action(), 0);
        press(ui, Key::Escape).await;
        assert!(!is_open(ui));
        // Live and unguarded, then Settings takes over: withdrawn unclicked.
        ui.set_live_session(true);
        settle(ui).await;
        assert!(!ui.get_matrix_link_protected());
        assert!(click("https://matrix.to/#/@alice:example.org"));
        settle(ui).await;
        assert!(is_open(ui));
        assert!(!ui.get_mention_ui_allowed(), "popups close under it");
        ui.set_page(1);
        settle(ui).await;
        assert!(!is_open(ui), "a protected surface withdrew the card");
        // Under another surface a click does nothing at all.
        assert!(click("https://matrix.to/#/@alice:example.org"));
        settle(ui).await;
        assert!(!is_open(ui));
        // A joined link's message still to come (its follow-up authority)
        // ends at a modal's own entry, synchronously, and closing that modal
        // never revives it: the quick switcher through its real open callback
        // (shortcuts::open_picker, registered by phase 14) and the account
        // menu through its real Slint entry. A route that skipped its entry
        // is ended by its first admission while the modal is up.
        ui.set_page(0);
        settle(ui).await;
        assert!(!ui.get_matrix_link_protected());
        let follow_up = || {
            let authority = NavigationAuthority::default().blocked_by(surface_blocked);
            FOLLOW_UP.with(|slot| *slot.borrow_mut() = Some(authority.clone()));
            assert!(authority.admits(ui));
            authority
        };
        let switching = follow_up();
        ui.invoke_open_quick_switcher();
        assert!(ui.get_quick_switcher_open(), "the real entry opened it");
        assert!(
            switching.withdrawn(),
            "ended at the entry, before any change handler"
        );
        ui.set_quick_switcher_open(false);
        settle(ui).await;
        assert!(!switching.admits(ui), "closing it never revives it");
        let menu = follow_up();
        ui.invoke_open_account_menu();
        assert!(ui.get_account_menu_open(), "the real entry opened it");
        assert!(menu.withdrawn(), "ended at the entry");
        ui.set_account_menu_open(false);
        settle(ui).await;
        assert!(!menu.admits(ui), "closing it never revives it");
        let missed = NavigationAuthority::default().blocked_by(surface_blocked);
        ui.set_room_actions_open(true);
        assert!(!missed.admits(ui), "admitted under a modal: ended");
        ui.set_room_actions_open(false);
        settle(ui).await;
        assert!(!missed.admits(ui), "and it stays ended");
        ui.set_page(page);
        ui.set_live_session(live_session);
        ui.set_show_sign_in(sign_in);
        settle(ui).await;
    }

    /// A joined link's message still to come, installed the way finish_join
    /// installs its follow-up authority (ended by the next clear and by any
    /// surface up at its admission). The thread browser's phase-89 case
    /// checks that the browser's own entry ends it.
    pub(crate) fn install_follow_up() -> NavigationAuthority {
        let authority = NavigationAuthority::default().blocked_by(surface_blocked);
        FOLLOW_UP.with(|slot| *slot.borrow_mut() = Some(authority.clone()));
        authority
    }

    /// A message link's confirmed join completing as the join's queued
    /// completion runs it (finish_join, then message_search's owned
    /// continuation), for `room_id` and `event_id` under `asked`. The opt-in
    /// live controller suite drives the actual continuation through it with
    /// a real selection and receipt; nothing is joined or sent here.
    pub(crate) fn finish_joined_link(
        ui: &AppWindow,
        live: &Arc<LiveUi>,
        runtime: &Handle,
        selection: AccountSelection,
        room_id: &str,
        event_id: &str,
        asked: &JoinAsked,
    ) {
        let operation = next_operation();
        let link = RoomLink {
            selection,
            navigation: live.current_navigation_epoch(),
            target: room(room_id, Some(event_id), &[]),
            reading: JoinCancellation::new(),
            preview: None,
            joining: Some(asked.clone()),
            open: None,
            presented: true,
        };
        SESSION.with(|session| {
            *session.borrow_mut() = Some(Session {
                operation,
                action: Action::None,
                shown: Shown::Room(link),
            });
        });
        finish_join(ui, live, runtime, operation, asked, Ok(true));
    }
}
