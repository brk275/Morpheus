//! A reply being typed survives leaving its room, for this session only.
//!
//! Replies (unlike room and thread drafts) have no saved draft, so a room
//! change used to drop the typed text silently. The window keeps the latest
//! reply per room of the visible account here, in memory, and offers it
//! again on returning to that room. Nothing is persisted or logged; an
//! account change, removal or sign-out forgets every kept reply. An edit
//! cannot be restored over its reloaded baseline, so leaving one is
//! reported instead.

use std::cell::RefCell;
use std::time::Duration;

use slint::{ComponentHandle, Timer};

use crate::AppWindow;

/// Rooms whose reply is kept at once; the oldest is forgotten first.
const MAX_KEPT: usize = 16;
const EDIT_NOT_KEPT: &str =
    "Your edit was not kept when you left the room; open the message to edit it again.";

struct KeptReply {
    account: String,
    room: String,
    target: String,
    label: String,
    text: String,
    markdown: bool,
}

thread_local! {
    static KEPT: RefCell<Vec<KeptReply>> = const { RefCell::new(Vec::new()) };
}

/// The visible room is about to change within the same account: keep a
/// reply being typed there.
pub(crate) fn leaving_room(ui: &AppWindow) {
    let account = ui.get_active_account_local_id().to_string();
    let room = ui.get_active_room_id().to_string();
    let text = ui.get_composer_text().to_string();
    if account.is_empty() || room.is_empty() || text.trim().is_empty() {
        return;
    }
    match ui.get_composer_mode() {
        1 => {}
        2 => {
            ui.set_transfer_notice(EDIT_NOT_KEPT.into());
            return;
        }
        _ => return,
    }
    let target = ui.get_composer_target_id().to_string();
    if target.is_empty() {
        return;
    }
    let reply = KeptReply {
        label: ui.get_composer_target_label().to_string(),
        markdown: ui.get_composer_markdown(),
        account,
        room,
        target,
        text,
    };
    KEPT.with(|kept| {
        let mut kept = kept.borrow_mut();
        kept.retain(|other| other.account != reply.account || other.room != reply.room);
        if kept.len() >= MAX_KEPT {
            kept.remove(0);
        }
        kept.push(reply);
    });
}

/// A room of the same account became visible: offer its kept reply again,
/// once the room's own draft and composer state are in place.
pub(crate) fn entered_room(ui: &AppWindow) {
    let account = ui.get_active_account_local_id().to_string();
    let room = ui.get_active_room_id().to_string();
    let reply = KEPT.with(|kept| {
        let mut kept = kept.borrow_mut();
        let at = kept
            .iter()
            .position(|reply| reply.account == account && reply.room == room)?;
        Some(kept.remove(at))
    });
    let Some(reply) = reply else {
        return;
    };
    let weak = ui.as_weak();
    Timer::single_shot(Duration::ZERO, move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let here = ui.get_active_account_local_id().as_str() == reply.account
            && ui.get_active_room_id().as_str() == reply.room;
        if !here || ui.get_composer_mode() != 0 || ui.get_composer_busy() {
            return;
        }
        // The window's own reply action: the room draft is parked and comes
        // back when the reply is sent or cancelled.
        ui.invoke_begin_compose(
            1,
            reply.target.into(),
            reply.label.into(),
            reply.text.into(),
            reply.markdown,
        );
    });
}

/// Forget every kept reply: the account changed, was removed or signed out.
pub(crate) fn forget_all() {
    KEPT.with(|kept| kept.borrow_mut().clear());
}
