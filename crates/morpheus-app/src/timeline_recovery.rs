//! A fixed notice for undecryptable entries in a loaded SDK timeline.
//! Loaded status retains its original account/view/security authority even
//! while Settings hides the view. Only the visible action is retired on a
//! page change; returning locally reissues a fresh serial if those original
//! leases still hold. Nothing requests keys, retries an event or sends a receipt.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use morpheus_matrix::AccountSelection;
use slint::Weak;

use crate::{AppWindow, security::SecurityGate};

#[derive(Clone, Copy)]
pub(crate) enum Pane {
    Room,
    Thread,
}

#[derive(Clone)]
pub(crate) struct Lease {
    pub(crate) selection: AccountSelection,
    pub(crate) room: String,
    pub(crate) root: Option<String>,
    pub(crate) focused: bool,
    pub(crate) navigation: Arc<AtomicU64>,
    pub(crate) navigation_at: u64,
    pub(crate) generation: Arc<AtomicU64>,
    pub(crate) generation_at: u64,
    pub(crate) live: Arc<AtomicBool>,
    pub(crate) gate: Arc<SecurityGate>,
}

impl Lease {
    fn holds(&self, ui: &AppWindow, security: u64) -> bool {
        self.selection.is_current()
            && self.live.load(Ordering::Acquire)
            && self.navigation.load(Ordering::Acquire) == self.navigation_at
            && self.generation.load(Ordering::Acquire) == self.generation_at
            && self.gate.ringing_allowed(security)
            && ui.get_active_account_local_id().as_str() == self.selection.account_id()
            && ui.get_active_room_id().as_str() == self.room
            && match &self.root {
                Some(root) => ui.get_thread_root_id().as_str() == root,
                None => ui.get_conversation_focused() == self.focused,
            }
    }
}

#[derive(Clone)]
struct Loaded {
    lease: Lease,
    security: u64,
}

/// One loaded status and the separate serial of its currently visible action.
/// `publication` identifies the retained status, never an account authority.
#[derive(Clone)]
struct Notice<T> {
    publication: i32,
    action: Option<i32>,
    loaded: T,
}

struct Book<T> {
    last: i32,
    room: Option<Notice<T>>,
    thread: Option<Notice<T>>,
}

impl<T> Default for Book<T> {
    fn default() -> Self {
        Self {
            last: 0,
            room: None,
            thread: None,
        }
    }
}

impl<T> Book<T> {
    fn slot(&mut self, pane: Pane) -> &mut Option<Notice<T>> {
        match pane {
            Pane::Room => &mut self.room,
            Pane::Thread => &mut self.thread,
        }
    }
    // Publication and action IDs share one monotonic space. Never reuse an
    // old callback ID, even after page roundtrips or status replacements.
    fn next(&mut self) -> Option<i32> {
        self.last = self.last.checked_add(1)?;
        Some(self.last)
    }
    fn replace(&mut self, pane: Pane, loaded: Option<T>) -> Option<i32> {
        let old = self.slot(pane).take().and_then(|notice| notice.action);
        if let Some(loaded) = loaded
            && let Some(publication) = self.next()
        {
            *self.slot(pane) = Some(Notice {
                publication,
                action: None,
                loaded,
            });
        }
        old
    }
    fn hide(&mut self, pane: Pane, publication: i32) -> Option<i32> {
        self.slot(pane)
            .as_mut()
            .filter(|notice| notice.publication == publication)
            .and_then(|notice| notice.action.take())
    }
    fn present(&mut self, pane: Pane, publication: i32) -> Option<i32> {
        let current = self
            .slot(pane)
            .as_ref()
            .filter(|notice| notice.publication == publication)?;
        if current.action.is_some() {
            return current.action;
        }
        let id = self.next()?;
        // No suspension or external callback between the check and install.
        self.slot(pane).as_mut()?.action = Some(id);
        Some(id)
    }
    fn retire(&mut self, pane: Pane, publication: i32) -> Option<i32> {
        self.slot(pane)
            .take_if(|notice| notice.publication == publication)
            .and_then(|notice| notice.action)
    }
    fn matches(&mut self, pane: Pane, id: i32) -> bool {
        id > 0
            && self
                .slot(pane)
                .as_ref()
                .is_some_and(|notice| notice.action == Some(id))
    }
}

/// Exactly two loaded statuses; no manager, client, store/profile lease,
/// strong window, message/error body, timer, task or waiting action is retained.
pub(crate) struct Notices {
    book: Mutex<Book<Loaded>>,
    ui: Mutex<Weak<AppWindow>>,
}

fn viewing(ui: &AppWindow) -> bool {
    ui.get_live_session() && ui.get_page() == 0
}

fn allowed(ui: &AppWindow) -> bool {
    viewing(ui) && ui.get_viewport_seek_allowed() && !ui.get_settings_reveal_protected()
}

fn shown_id(ui: &AppWindow, pane: Pane) -> i32 {
    match pane {
        Pane::Room => ui.get_timeline_recovery_serial(),
        Pane::Thread => ui.get_thread_recovery_serial(),
    }
}

fn show(ui: &AppWindow, pane: Pane, id: i32) {
    match pane {
        Pane::Room => ui.set_timeline_recovery_serial(id),
        Pane::Thread => ui.set_thread_recovery_serial(id),
    }
}

fn clear_shown(ui: &AppWindow, pane: Pane, old: Option<i32>) {
    if old.is_some_and(|id| shown_id(ui, pane) == id) {
        show(ui, pane, 0);
    }
}

impl Notices {
    /// On LiveUi's creation thread. Headless instances register nothing.
    /// Callbacks retain only this book and a weak window, never LiveUi.
    pub(crate) fn new(weak: Weak<AppWindow>) -> Arc<Self> {
        let notices = Arc::new(Self {
            book: Mutex::new(Book::default()),
            ui: Mutex::new(weak.clone()),
        });
        if let Some(ui) = weak.upgrade() {
            let current = notices.clone();
            let open_ui = weak.clone();
            ui.on_open_timeline_recovery(move |thread, id| {
                if let Some(ui) = open_ui.upgrade() {
                    current.open(&ui, if thread { Pane::Thread } else { Pane::Room }, id);
                }
            });
            let current = notices.clone();
            ui.on_timeline_recovery_context_changed(move || {
                if let Some(ui) = weak.upgrade() {
                    current.sweep(&ui);
                }
            });
        }
        notices
    }

    fn book(&self) -> std::sync::MutexGuard<'_, Book<Loaded>> {
        self.book.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Alongside the actual rows, including SDK publications while the page
    /// is hidden. Retain the latest status only under its original scope and
    /// captured security epoch. Page visibility never discards loaded data.
    pub(crate) fn publish(
        &self,
        ui: &AppWindow,
        pane: Pane,
        missing: bool,
        lease: Lease,
        security: u64,
    ) {
        // A stale publisher has no authority to erase a newer retained status.
        // The actual callers also retain their original publication guards.
        if !ui.get_live_session() || !lease.holds(ui, security) {
            self.sweep(ui);
            return;
        }
        let loaded = missing.then_some(Loaded { lease, security });
        let old = self.book().replace(pane, loaded);
        clear_shown(ui, pane, old);
        self.sync(ui, pane);
    }

    /// Navigation/feed invalidation retires BOTH the retained status and its
    /// action synchronously. Late visual cleanup only clears that old serial.
    pub(crate) fn clear(&self, pane: Pane) {
        let old = self
            .book()
            .slot(pane)
            .take()
            .and_then(|notice| notice.action);
        self.clear_later(pane, old);
    }

    fn clear_later(&self, pane: Pane, old: Option<i32>) {
        if old.is_some() {
            let weak = self
                .ui
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                clear_shown(&ui, pane, old);
            });
        }
    }

    pub(crate) fn clear_all(&self) {
        self.clear(Pane::Room);
        self.clear(Pane::Thread);
    }

    /// Successful explicit entry retires only the visible callbacks. The
    /// exact loaded status is kept for a later local return from Settings.
    fn hide_all(&self, ui: &AppWindow) {
        for pane in [Pane::Room, Pane::Thread] {
            let notice = self.book().slot(pane).clone();
            if let Some(notice) = notice {
                let old = self.book().hide(pane, notice.publication);
                clear_shown(ui, pane, old);
            }
        }
    }

    fn sweep(&self, ui: &AppWindow) {
        self.sync(ui, Pane::Room);
        self.sync(ui, Pane::Thread);
    }

    /// Scope checks happen outside the book lock. Apply only to that exact
    /// publication after the check. No account/security lease is resampled
    /// here; a changed/invalid scope destroys the retained status for good.
    fn sync(&self, ui: &AppWindow, pane: Pane) {
        let notice = self.book().slot(pane).clone();
        let Some(notice) = notice else {
            return;
        };
        if !ui.get_live_session() || !notice.loaded.lease.holds(ui, notice.loaded.security) {
            let old = self.book().retire(pane, notice.publication);
            clear_shown(ui, pane, old);
        } else if !viewing(ui) {
            let old = self.book().hide(pane, notice.publication);
            clear_shown(ui, pane, old);
        } else {
            // Returning to chat needs no SDK diff. A hidden action gets a
            // new ID; an already shown valid action keeps its current ID.
            let id = self.book().present(pane, notice.publication);
            if let Some(id) = id {
                show(ui, pane, id);
            }
        }
    }

    fn open(&self, ui: &AppWindow, pane: Pane, id: i32) {
        let notice = self.book().slot(pane).clone();
        let Some(notice) = notice else {
            return;
        };
        // An old/forged serial leaves a newer valid owner/action unchanged.
        let current_action = self.book().matches(pane, id);
        if notice.action != Some(id) || !current_action || shown_id(ui, pane) != id {
            return;
        }
        if !allowed(ui)
            || !notice.loaded.lease.holds(ui, notice.loaded.security)
            || (matches!(pane, Pane::Thread)
                && (!ui.get_show_side_panel() || ui.get_side_panel_tab() != 1))
        {
            self.sweep(ui);
            return;
        }
        self.hide_all(ui);
        crate::withdraw_call_bar(ui);
        // Static existing destination; no event/key request, retry or receipt.
        if crate::settings_search::reveal(ui, "security.recovery") {
            ui.invoke_open_security();
        } else {
            // An independently refused reveal never consumes loaded status.
            self.sweep(ui);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_recovery_presentation_cannot_adopt_a_new_security_epoch() {
        let gate = Arc::new(SecurityGate::default());
        let delivery = crate::timeline_delivery::Latest::new();
        let original = crate::timeline_viewport::security_generation(&gate);
        assert!(gate.ringing_allowed(original));
        assert!(delivery.submit((true, original)));
        let step = gate.try_account_access().expect("idle gate");
        drop(step);
        let (missing, captured) = delivery.take().unwrap();
        assert!(missing);
        assert_eq!(captured, original);
        assert!(
            !gate.ringing_allowed(captured),
            "a completed security step still retires the queued epoch"
        );
        let current = crate::timeline_viewport::security_generation(&gate);
        assert!(delivery.submit((false, current)));
        assert!(
            !delivery.submit((true, current)),
            "one pending callback carries the latest status and its epoch together"
        );
        let (missing, captured) = delivery.take().unwrap();
        assert!(missing && gate.ringing_allowed(captured));
        assert_eq!(captured, current);
        assert!(delivery.take().is_none());
    }

    #[test]
    fn recovery_notice_serial_never_reuses_an_old_callback() {
        let mut book = Book::<()>::default();
        assert_eq!(book.next(), Some(1));
        assert_eq!(book.next(), Some(2));
        book.last = i32::MAX - 1;
        assert_eq!(book.next(), Some(i32::MAX));
        assert_eq!(book.next(), None);
        assert_eq!(book.next(), None);
    }

    #[test]
    fn loaded_status_survives_page_roundtrip_with_a_new_action_serial() {
        let mut book = Book::default();
        book.replace(Pane::Room, Some(("original-selection", 7u64, 11u64)));
        let publication = book.room.as_ref().unwrap().publication;
        let first = book.present(Pane::Room, publication).unwrap();
        assert!(book.matches(Pane::Room, first));
        assert_eq!(book.hide(Pane::Room, publication), Some(first));
        assert_eq!(
            book.room.as_ref().unwrap().loaded,
            ("original-selection", 7, 11)
        );
        assert!(!book.matches(Pane::Room, first));
        let returned = book.present(Pane::Room, publication).unwrap();
        assert!(returned > first);
        assert!(!book.matches(Pane::Room, first));
        assert!(book.matches(Pane::Room, returned));
        assert_eq!(
            book.present(Pane::Room, publication),
            Some(returned),
            "a deferred context check must not replace a current action"
        );
    }

    #[test]
    fn hidden_sdk_replacement_keeps_latest_status_and_stale_cleanup_cannot_remove_it() {
        let mut book = Book::default();
        book.replace(Pane::Room, Some("old loaded status"));
        let old_publication = book.room.as_ref().unwrap().publication;
        let old_action = book.present(Pane::Room, old_publication).unwrap();
        book.hide(Pane::Room, old_publication);
        book.replace(Pane::Room, Some("latest original publication"));
        let current = book.room.as_ref().unwrap().publication;
        assert!(
            book.room.as_ref().unwrap().action.is_none(),
            "hidden data needs no visible action"
        );
        assert_eq!(book.retire(Pane::Room, old_publication), None);
        let returned = book.present(Pane::Room, current).unwrap();
        assert!(returned > old_action);
        assert_eq!(
            book.room.as_ref().unwrap().loaded,
            "latest original publication"
        );
        assert!(!book.matches(Pane::Room, old_action));
        assert_eq!(book.present(Pane::Room, current), Some(returned));
        book.replace(Pane::Room, None);
        assert!(
            book.room.is_none(),
            "redecryption/removal while hidden stays cleared on return"
        );
        assert_eq!(book.present(Pane::Room, current), None);
    }

    #[test]
    fn own_recovery_entry_hides_actions_without_consuming_loaded_authority() {
        let mut book = Book::default();
        book.replace(Pane::Room, Some("original room lease"));
        book.replace(Pane::Thread, Some("original thread lease"));
        let room = book.room.as_ref().unwrap().publication;
        let thread = book.thread.as_ref().unwrap().publication;
        let old_room = book.present(Pane::Room, room).unwrap();
        let old_thread = book.present(Pane::Thread, thread).unwrap();
        // The very same hides used by successful entry, then page sync.
        assert_eq!(book.hide(Pane::Room, room), Some(old_room));
        assert_eq!(book.hide(Pane::Thread, thread), Some(old_thread));
        assert_eq!(book.hide(Pane::Room, room), None);
        assert_eq!(book.room.as_ref().unwrap().loaded, "original room lease");
        assert_eq!(
            book.thread.as_ref().unwrap().loaded,
            "original thread lease"
        );
        assert!(book.present(Pane::Room, room).unwrap() > old_room);
        assert!(book.present(Pane::Thread, thread).unwrap() > old_thread);
        assert!(!book.matches(Pane::Room, old_room));
        assert!(!book.matches(Pane::Thread, old_thread));
        book.retire(Pane::Room, room);
        book.retire(Pane::Thread, thread);
        assert_eq!(
            book.present(Pane::Room, room),
            None,
            "invalid scope is never adopted on return"
        );
        assert_eq!(book.present(Pane::Thread, thread), None);
    }
}
