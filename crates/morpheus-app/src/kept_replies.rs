//! A reply being typed survives leaving its room, for this session only.
//!
//! Replies (unlike room and thread drafts) have no saved draft, so a room
//! change used to drop the typed text silently. The window keeps the latest
//! reply per room of the visible account here, in memory, and offers it
//! again on returning to that room. Nothing is persisted or logged; an
//! account change, removal or sign-out forgets every kept reply. An edit
//! cannot be restored over its reloaded baseline, so leaving one is
//! reported instead.
//!
//! The text stays in this store until a restore actually succeeds. A
//! deferred restore holds only the store's epoch and the room's IDs; every
//! room change and every clear moves the epoch synchronously, so a queued
//! restore from before them does nothing, and a busy or stale attempt
//! consumes nothing.

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

/// The kept replies and the epoch that fences their deferred restores.
#[derive(Default)]
struct Store {
    epoch: u64,
    kept: Vec<KeptReply>,
}

impl Store {
    /// The visible room is changing: earlier queued restores are void.
    fn room_changing(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    fn keep(&mut self, reply: KeptReply) {
        self.kept
            .retain(|other| other.account != reply.account || other.room != reply.room);
        if self.kept.len() >= MAX_KEPT {
            self.kept.remove(0);
        }
        self.kept.push(reply);
    }

    /// The epoch a restore of this room's reply must still match, if one is
    /// kept. The text stays here.
    fn pending(&self, account: &str, room: &str) -> Option<u64> {
        let kept = self.kept.iter();
        let mut matching = kept.filter(|reply| reply.account == account && reply.room == room);
        matching.next().map(|_| self.epoch)
    }

    /// Hand the reply to a restore that is still current: same epoch, same
    /// account and room. Anything else takes nothing.
    fn take(&mut self, epoch: u64, account: &str, room: &str) -> Option<KeptReply> {
        if epoch != self.epoch {
            return None;
        }
        let at = self
            .kept
            .iter()
            .position(|reply| reply.account == account && reply.room == room)?;
        Some(self.kept.remove(at))
    }

    fn forget_all(&mut self) {
        self.kept.clear();
        self.epoch = self.epoch.wrapping_add(1);
    }
}

thread_local! {
    static STORE: RefCell<Store> = RefCell::new(Store::default());
}

fn with<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    STORE.with(|store| f(&mut store.borrow_mut()))
}

/// The visible room is about to change within the same account: void any
/// queued restore and keep a reply being typed in the room being left.
pub(crate) fn leaving_room(ui: &AppWindow) {
    with(Store::room_changing);
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
        account,
        room,
        target,
        label: ui.get_composer_target_label().to_string(),
        text,
        markdown: ui.get_composer_markdown(),
    };
    with(|store| store.keep(reply));
}

/// A room of the same account became visible: offer its kept reply again
/// once the room's own draft and composer state are in place. The queued
/// restore holds only the epoch and the room's IDs.
pub(crate) fn entered_room(ui: &AppWindow) {
    let account = ui.get_active_account_local_id().to_string();
    let room = ui.get_active_room_id().to_string();
    let Some(epoch) = with(|store| store.pending(&account, &room)) else {
        return;
    };
    let weak = ui.as_weak();
    Timer::single_shot(Duration::ZERO, move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let here = ui.get_active_account_local_id().as_str() == account
            && ui.get_active_room_id().as_str() == room;
        // A busy or changed composer takes nothing; the reply stays kept.
        if !here || ui.get_composer_mode() != 0 || ui.get_composer_busy() {
            return;
        }
        let Some(reply) = with(|store| store.take(epoch, &account, &room)) else {
            return;
        };
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

/// Forget every kept reply and void queued restores: the account changed,
/// was removed or signed out.
pub(crate) fn forget_all() {
    with(Store::forget_all);
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = "account-a";
    const ROOM: &str = "!room:example.org";

    fn reply(room: &str) -> KeptReply {
        KeptReply {
            account: ACCOUNT.into(),
            room: room.into(),
            target: "$target:example.org".into(),
            label: "Casey".into(),
            text: "reply text".into(),
            markdown: false,
        }
    }

    #[test]
    fn a_queued_restore_cannot_outlive_a_clear_or_a_room_change() {
        let mut store = Store::default();
        store.keep(reply(ROOM));
        // Entering the room queues a restore; the account is cleared first.
        let queued = store.pending(ACCOUNT, ROOM).unwrap();
        store.forget_all();
        assert!(store.take(queued, ACCOUNT, ROOM).is_none());
        assert!(store.kept.is_empty());

        // A room change before the queued restore runs voids it but keeps
        // the text for the next return; another account takes nothing.
        store.keep(reply(ROOM));
        let queued = store.pending(ACCOUNT, ROOM).unwrap();
        store.room_changing();
        assert!(store.take(queued, ACCOUNT, ROOM).is_none());
        assert_eq!(store.kept.len(), 1);
        let current = store.pending(ACCOUNT, ROOM).unwrap();
        assert!(store.take(current, "account-b", ROOM).is_none());
        let restored = store.take(current, ACCOUNT, ROOM);
        assert_eq!(restored.map(|reply| reply.text), Some("reply text".into()));
        assert!(store.kept.is_empty());

        // The store stays bounded.
        for index in 0..MAX_KEPT + 3 {
            store.keep(reply(&format!("!room{index}:example.org")));
        }
        assert_eq!(store.kept.len(), MAX_KEPT);
    }
}
