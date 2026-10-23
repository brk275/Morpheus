//! Scopes for appearance work that outlives one UI callback: a theme pack
//! import (file dialog, preview, Apply), a theme pack export (file dialog,
//! write) and a studio Apply waiting to be tried again.
//!
//! Such work captures its [`Scope`] before its first asynchronous step: the
//! appearance epoch, the active account and an operation number. Its later
//! steps act only while that scope is still current. The epoch is
//! monotonic. It moves on:
//! - every change of the active account seen by the studio, including a
//!   detach to no account; so A -> B -> A is two new epochs, never the old
//!   one again;
//! - every new settings context (the studio registering a store).
//!
//! Changes are observed in two places:
//! - the account hook that every account publication and detach runs
//!   (`appearance::apply_theme_mode`, then `theme_studio::check_account`,
//!   then `packs::check_account`);
//! - every validation. Each check re-reads the window's active account
//!   first, so a change no hook reported still ends the scope.
//!
//! Only an A -> B -> A that happens entirely between two observations, with
//! no hook run for B, could go unseen. Production account publications all
//! run the hook.
//!
//! A native file dialog cannot be cancelled from here: rfd offers no
//! cancel. When its scope ends, the dialog stays open, and its answer is
//! ignored when it arrives. The dialog slot is held by one operation at a
//! time.
//!
//! Offline settings (no account active) are an ordinary scope with an empty
//! account.
//!
//! Room-scoped work (editing one room's appearance) also captures the active
//! room and a separate monotonic room epoch. It moves on every change of the
//! window's `active-room-id`: the room hook (`changed active-room-id`) and
//! every check observe it. A room scope ends on a room change, A -> B -> A
//! included, and on anything that ends its account scope. Account-wide work
//! (theme packs, style profiles) ignores room changes.

use std::cell::RefCell;

use crate::AppWindow;

/// What a piece of asynchronous appearance work belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Scope {
    epoch: u64,
    account: String,
    operation: u64,
    /// For room-scoped work: the room epoch and room ID it was captured in.
    room: Option<(u64, String)>,
}

impl Scope {
    /// The account the work belongs to; empty in offline settings.
    pub(crate) fn account(&self) -> &str {
        &self.account
    }

    /// The room room-scoped work belongs to.
    pub(crate) fn room(&self) -> Option<&str> {
        self.room.as_ref().map(|(_, room)| room.as_str())
    }
}

/// The current epoch, the account last observed and the operation whose
/// file dialog is open. Pure, so the fence rules are unit tested here.
#[derive(Debug, Default)]
pub(crate) struct Scopes {
    closed: bool,
    epoch: u64,
    /// `None` before the first observation.
    observed: Option<String>,
    next_operation: u64,
    dialog: Option<u64>,
    room_epoch: u64,
    /// `None` before the first room observation.
    observed_room: Option<String>,
}

impl Scopes {
    fn close(&mut self) {
        self.closed = true;
        self.invalidate();
    }

    /// Record the active account. Any change starts a new epoch and drops
    /// the open dialog's claim.
    pub(crate) fn observe(&mut self, account: &str) {
        if self.observed.as_deref() != Some(account) {
            self.epoch = self.epoch.wrapping_add(1);
            self.observed = Some(account.to_owned());
            self.dialog = None;
        }
    }

    /// Record the active room. Any change starts a new room epoch.
    pub(crate) fn observe_room(&mut self, room: &str) {
        if self.observed_room.as_deref() != Some(room) {
            self.room_epoch = self.room_epoch.wrapping_add(1);
            self.observed_room = Some(room.to_owned());
        }
    }

    /// A new settings context: everything captured before is stale.
    pub(crate) fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.dialog = None;
    }

    /// The scope of work starting now in `account`.
    pub(crate) fn capture(&mut self, account: &str) -> Scope {
        self.observe(account);
        self.next_operation = self.next_operation.wrapping_add(1);
        Scope {
            epoch: self.epoch,
            account: account.to_owned(),
            operation: self.next_operation,
            room: None,
        }
    }

    /// The scope of work for `room` while `account` is active.
    pub(crate) fn capture_room(&mut self, account: &str, room: &str) -> Scope {
        self.observe_room(room);
        let mut scope = self.capture(account);
        scope.room = Some((self.room_epoch, room.to_owned()));
        scope
    }

    /// [`Scopes::capture`] for work that opens a file dialog. A newer dialog
    /// takes the slot from an older one.
    pub(crate) fn begin_dialog(&mut self, account: &str) -> Scope {
        let scope = self.capture(account);
        self.dialog = Some(scope.operation);
        scope
    }

    /// Whether `scope` is still current while `account` (and `room`) are
    /// active: the same epoch, so no account change or new settings context
    /// since, and the same account. A room scope also needs the same room
    /// epoch and room.
    pub(crate) fn current(&mut self, scope: &Scope, account: &str, room: &str) -> bool {
        self.observe(account);
        self.observe_room(room);
        let room_current = scope
            .room
            .as_ref()
            .is_none_or(|(epoch, captured)| *epoch == self.room_epoch && captured == room);
        !self.closed && scope.epoch == self.epoch && scope.account == account && room_current
    }

    /// The dialog opened in `scope` returned. Releases its slot and says
    /// whether its answer may be used: it must still hold the slot (no newer
    /// dialog, no change since), and its scope must be current. A second
    /// delivery of the same answer is refused.
    pub(crate) fn finish_dialog(&mut self, scope: &Scope, account: &str, room: &str) -> bool {
        let held = self.dialog == Some(scope.operation);
        if held {
            self.dialog = None;
        }
        held && self.current(scope, account, room)
    }
}

thread_local! {
    // Slint callbacks and hooks run on the UI thread only.
    static SCOPES: RefCell<Scopes> = RefCell::new(Scopes::default());
}

fn with<R>(update: impl FnOnce(&mut Scopes) -> R) -> R {
    SCOPES.with(|scopes| update(&mut scopes.borrow_mut()))
}

fn active(ui: &AppWindow) -> String {
    ui.get_active_account_local_id().to_string()
}

fn active_room(ui: &AppWindow) -> String {
    ui.get_active_room_id().to_string()
}

/// Observe the window's active account and room (the account and room
/// hooks).
pub(crate) fn observe(ui: &AppWindow) {
    let (account, room) = (active(ui), active_room(ui));
    with(|scopes| {
        scopes.observe(&account);
        scopes.observe_room(&room);
    });
}

/// Permanently retire this application session, including queued answers.
pub(crate) fn close() {
    with(Scopes::close);
}

/// A new settings context.
pub(crate) fn invalidate() {
    with(Scopes::invalidate);
}

/// The scope of work starting now.
pub(crate) fn capture(ui: &AppWindow) -> Scope {
    let account = active(ui);
    with(|scopes| scopes.capture(&account))
}

/// The scope of a file dialog opening now.
pub(crate) fn begin_dialog(ui: &AppWindow) -> Scope {
    let account = active(ui);
    with(|scopes| scopes.begin_dialog(&account))
}

/// The scope of work for the window's active room starting now.
pub(crate) fn capture_room(ui: &AppWindow) -> Scope {
    let (account, room) = (active(ui), active_room(ui));
    with(|scopes| scopes.capture_room(&account, &room))
}

/// Whether `scope` is still current in the window now.
pub(crate) fn current(ui: &AppWindow, scope: &Scope) -> bool {
    let (account, room) = (active(ui), active_room(ui));
    with(|scopes| scopes.current(scope, &account, &room))
}

/// The dialog of `scope` returned: whether its answer may be used now.
pub(crate) fn finish_dialog(ui: &AppWindow, scope: &Scope) -> bool {
    let (account, room) = (active(ui), active_room(ui));
    with(|scopes| scopes.finish_dialog(scope, &account, &room))
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    with(|scopes| *scopes = Scopes::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_close_invalidates_queued_answers_and_cannot_be_reopened() {
        let mut scopes = Scopes::default();
        let old = scopes.begin_dialog("account");
        scopes.close();
        assert!(!scopes.finish_dialog(&old, "account", "room"));
        scopes.invalidate();
        let later = scopes.begin_dialog("account");
        assert!(!scopes.finish_dialog(&later, "account", "room"));
        let later = scopes.capture_room("account", "room");
        assert!(!scopes.current(&later, "account", "room"));
    }

    #[test]
    fn a_to_b_to_a_ends_a_dialog_and_a_scope() {
        let mut scopes = Scopes::default();
        let dialog = scopes.begin_dialog("a");
        let preview = scopes.capture("a");
        scopes.observe("b");
        scopes.observe("a");
        // The account string matches again, but the epoch does not.
        assert!(!scopes.current(&preview, "a", ""));
        assert!(!scopes.finish_dialog(&dialog, "a", ""));
    }

    #[test]
    fn a_change_no_hook_reported_is_seen_by_the_check_itself() {
        let mut scopes = Scopes::default();
        let scope = scopes.capture("a");
        // No observe("b") happened; the check reads the account itself.
        assert!(!scopes.current(&scope, "b", ""));
        // And returning to "a" afterwards is still a new epoch.
        assert!(!scopes.current(&scope, "a", ""));
    }

    #[test]
    fn an_unchanged_scope_stays_current_and_a_dialog_answers_once() {
        let mut scopes = Scopes::default();
        let dialog = scopes.begin_dialog("a");
        scopes.observe("a");
        assert!(scopes.current(&dialog, "a", ""));
        assert!(scopes.finish_dialog(&dialog, "a", ""));
        // A late second delivery of the same answer is refused.
        assert!(!scopes.finish_dialog(&dialog, "a", ""));
        // The scope itself is still current for work it already started.
        assert!(scopes.current(&dialog, "a", ""));
    }

    #[test]
    fn a_newer_dialog_takes_the_slot() {
        let mut scopes = Scopes::default();
        let older = scopes.begin_dialog("a");
        let newer = scopes.begin_dialog("a");
        assert!(!scopes.finish_dialog(&older, "a", ""));
        assert!(scopes.finish_dialog(&newer, "a", ""));
    }

    #[test]
    fn detach_and_offline_settings_are_scopes_too() {
        let mut scopes = Scopes::default();
        let attached = scopes.begin_dialog("a");
        scopes.observe("");
        assert!(!scopes.finish_dialog(&attached, "", ""));
        // With no account active, settings still work in their own scope.
        let offline = scopes.begin_dialog("");
        assert!(scopes.finish_dialog(&offline, "", ""));
        let preview = scopes.capture("");
        assert!(scopes.current(&preview, "", ""));
        // Signing in ends the offline scope.
        assert!(!scopes.current(&preview, "a", ""));
    }

    #[test]
    fn a_new_settings_context_ends_everything() {
        let mut scopes = Scopes::default();
        let dialog = scopes.begin_dialog("a");
        let preview = scopes.capture("a");
        scopes.invalidate();
        assert!(!scopes.current(&preview, "a", ""));
        assert!(!scopes.finish_dialog(&dialog, "a", ""));
    }

    #[test]
    fn a_room_change_ends_a_room_scope_but_not_account_wide_work() {
        let mut scopes = Scopes::default();
        let room = scopes.capture_room("a", "!r:example.org");
        let account_wide = scopes.capture("a");
        scopes.observe_room("!s:example.org");
        scopes.observe_room("!r:example.org");
        // Room R again, but a new room epoch.
        assert!(!scopes.current(&room, "a", "!r:example.org"));
        assert!(scopes.current(&account_wide, "a", "!r:example.org"));
    }

    #[test]
    fn a_room_scope_ends_with_its_account_scope() {
        let mut scopes = Scopes::default();
        let room = scopes.capture_room("a", "!r:example.org");
        assert!(scopes.current(&room, "a", "!r:example.org"));
        // A room change no hook reported is seen by the check itself.
        assert!(!scopes.current(&room, "a", "!s:example.org"));
        let room = scopes.capture_room("a", "!r:example.org");
        scopes.observe("b");
        scopes.observe("a");
        assert!(!scopes.current(&room, "a", "!r:example.org"));
        assert_eq!(room.room(), Some("!r:example.org"));
        assert_eq!(room.account(), "a");
    }
}
